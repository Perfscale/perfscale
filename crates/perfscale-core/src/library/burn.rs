//! Burn — ahead-of-time precompilation of WASM library components (RFC 005).
//!
//! Two phases sharing one artifact format:
//!
//! - **Burn cache** (`perfscale install` writes, the loader reads): each
//!   library is precompiled to native code once (`Engine::precompile_component`)
//!   and stored as `<cache>/libraries/<sha256>.cwasm`. At run time
//!   [`WasmLibraryProvider::load`](super::wasm::WasmLibraryProvider) probes
//!   this cache and deserializes the artifact — mmap of native code instead
//!   of seconds of Cranelift compilation per run.
//! - **Binary embedding** (`perfscale burn` writes, [`embedded`] reads): the
//!   `.cwasm` artifacts of a document's libraries are appended to a copy of
//!   the perfscale binary behind a fixed-size trailer, so the derived binary
//!   resolves its libraries from itself — one file, zero disk dependencies.
//!
//! # Artifact format (`PFSBURN1`)
//!
//! ```text
//! magic "PFSBURN1" | format_version u32 | wasmtime_version string |
//! target_triple string | engine_tag u64 | source_sha256 [32] | payload
//! ```
//!
//! (`string` = u32 length + bytes, integers little-endian; `payload` is the
//! output of `Component::serialize`.)
//!
//! `Component::deserialize` is `unsafe`: wasmtime only guarantees rejection
//! of incompatible artifacts, trusting the *contents* is on us. We therefore
//! only accept artifacts wrapped in this header and matching every field —
//! produced by this perfscale build, for this target, with this engine
//! configuration, from exactly these source bytes. Any mismatch in the
//! **cache** path silently falls back to full compilation; in the
//! **embedded** path it is a hard error ("re-burn with this perfscale
//! version"), because a shipped binary has no fallback.
//!
//! # Embedded trailer format (`PFSEMBED`)
//!
//! Appended after the unmodified binary image; the trailer sits at the very
//! end of the file so it works identically for ELF, Mach-O, and PE:
//!
//! ```text
//! payload  = count u32 | per library: use_len u32 + use_bytes |
//!            source_sha256 [32] | cwasm_len u64 + cwasm_bytes
//! trailer  = magic "PFSEMBED" | version u32 | payload_offset u64 |
//!            payload_len u64 | payload_sha256 [32]        (60 bytes, at EOF)
//! ```

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use wasmtime::component::Component;

/// Format version of the `PFSBURN1` artifact header.
pub const BURN_FORMAT_VERSION: u32 = 1;
/// Format version of the `PFSEMBED` trailer.
pub const EMBED_FORMAT_VERSION: u32 = 1;

const ARTIFACT_MAGIC: &[u8; 8] = b"PFSBURN1";
const EMBED_MAGIC: &[u8; 8] = b"PFSEMBED";
/// magic | version | payload_offset | payload_len | payload_sha256.
const TRAILER_LEN: usize = 8 + 4 + 8 + 8 + 32;

/// sha256 of raw bytes.
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Lowercase hex of a sha256 digest — the cache key form.
pub fn hex(sha: &[u8; 32]) -> String {
    sha.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of raw bytes, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256_bytes(bytes))
}

/// Tag of the engine configuration a precompiled artifact depends on. The
/// shared engine (`wasm::engine`) is built with `consume_fuel(true)` and
/// nothing else; **any change to that `Config` must be reflected here**, or
/// stale artifacts would load against different compilation settings.
fn engine_tag() -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in b"consume_fuel=1" {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Precompile a component to a burn artifact (header + serialized native
/// image). Compile errors surface exactly like a plain load would.
pub fn burn_component(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let engine = super::wasm::engine()?;
    let payload = engine
        .precompile_component(bytes)
        .map_err(|e| format!("failed to precompile the WASM component: {e}"))?;
    let mut out = Vec::with_capacity(payload.len() + 128);
    out.extend_from_slice(ARTIFACT_MAGIC);
    out.extend_from_slice(&BURN_FORMAT_VERSION.to_le_bytes());
    put_str(&mut out, env!("PERFSCALE_WASMTIME_VERSION"));
    put_str(&mut out, env!("PERFSCALE_TARGET_TRIPLE"));
    out.extend_from_slice(&engine_tag().to_le_bytes());
    out.extend_from_slice(&sha256_bytes(bytes));
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserialize a burn artifact produced from source bytes digesting to
/// `source_sha256`. Any header mismatch, truncation, or deserialization
/// failure is `None` — the caller falls back to full compilation (cache) or
/// turns it into a hard error (embedded).
pub fn load_burned(artifact: &[u8], source_sha256: &[u8; 32]) -> Option<Component> {
    let payload = parse_header(artifact, source_sha256)?;
    let engine = super::wasm::engine().ok()?;
    // SAFETY: the header was fully validated above — our magic, the exact
    // format version, the wasmtime version and target triple this build runs
    // on, the engine configuration tag, and the sha256 of the source
    // component — so the payload can only be an artifact this build (or an
    // identical one) produced. wasmtime additionally re-checks its own
    // artifact metadata inside `deserialize`.
    unsafe { Component::deserialize(engine, payload) }.ok()
}

/// Whether `artifact` is a valid burn artifact for `source_sha256` —
/// header check only, without deserializing. Used by `perfscale install` to
/// skip re-burning an up-to-date cache entry.
pub fn artifact_matches(artifact: &[u8], source_sha256: &[u8; 32]) -> bool {
    parse_header(artifact, source_sha256).is_some()
}

/// Validate the header, returning the payload slice on a full match.
fn parse_header<'a>(artifact: &'a [u8], source_sha256: &[u8; 32]) -> Option<&'a [u8]> {
    let mut r = Reader(artifact);
    if r.take(8)? != ARTIFACT_MAGIC {
        return None;
    }
    if r.u32()? != BURN_FORMAT_VERSION {
        return None;
    }
    if r.str()? != env!("PERFSCALE_WASMTIME_VERSION").as_bytes() {
        return None;
    }
    if r.str()? != env!("PERFSCALE_TARGET_TRIPLE").as_bytes() {
        return None;
    }
    if r.u64()? != engine_tag() {
        return None;
    }
    if r.take(32)? != source_sha256 {
        return None;
    }
    let payload = r.rest();
    if payload.is_empty() {
        return None;
    }
    Some(payload)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

// ---------------------------------------------------------------------------
// Embedded payload (`perfscale burn`)
// ---------------------------------------------------------------------------

/// One library embedded in a burned binary: the precompiled artifact plus
/// the sha256 of the source `.wasm` it was burned from.
#[derive(Debug, Clone)]
pub struct EmbeddedLibrary {
    pub source_sha256: [u8; 32],
    pub artifact: Vec<u8>,
}

/// The libraries embedded in this binary, keyed by the exact `use:` string
/// the resolved YAML documents carry (anchored path or cache artifact path).
#[derive(Debug, Default)]
pub struct EmbeddedSet {
    libraries: HashMap<String, EmbeddedLibrary>,
}

impl EmbeddedSet {
    pub fn get(&self, use_: &str) -> Option<&EmbeddedLibrary> {
        self.libraries.get(use_)
    }

    pub fn len(&self) -> usize {
        self.libraries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.libraries.is_empty()
    }
}

/// The payload embedded in this binary, if any — read once from
/// `current_exe`'s trailer. `None` for ordinary (unburned) binaries and for
/// any inconsistency; the disk/cache resolution path then applies unchanged.
pub fn embedded() -> &'static Option<EmbeddedSet> {
    static EMBEDDED: OnceLock<Option<EmbeddedSet>> = OnceLock::new();
    EMBEDDED.get_or_init(read_embedded)
}

fn read_embedded() -> Option<EmbeddedSet> {
    let exe = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len < TRAILER_LEN as u64 {
        return None;
    }
    file.seek(SeekFrom::End(-(TRAILER_LEN as i64))).ok()?;
    let mut trailer = [0u8; TRAILER_LEN];
    file.read_exact(&mut trailer).ok()?;
    let (offset, len, sha) = parse_trailer(&trailer)?;
    // The payload must end exactly where the trailer begins.
    if offset.checked_add(len)? != file_len - TRAILER_LEN as u64 {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut payload = vec![0u8; len as usize];
    file.read_exact(&mut payload).ok()?;
    if sha256_bytes(&payload) != sha {
        return None;
    }
    parse_payload(&payload)
}

/// Serialize the embedded payload (everything between the binary image and
/// the trailer).
pub fn encode_embedded_payload(libs: &[(String, EmbeddedLibrary)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(libs.len() as u32).to_le_bytes());
    for (use_, lib) in libs {
        out.extend_from_slice(&(use_.len() as u32).to_le_bytes());
        out.extend_from_slice(use_.as_bytes());
        out.extend_from_slice(&lib.source_sha256);
        out.extend_from_slice(&(lib.artifact.len() as u64).to_le_bytes());
        out.extend_from_slice(&lib.artifact);
    }
    out
}

/// Serialize the 60-byte trailer for `payload` appended at `payload_offset`
/// (the size of the binary image the payload follows).
pub fn encode_embedded_trailer(payload: &[u8], payload_offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(TRAILER_LEN);
    out.extend_from_slice(EMBED_MAGIC);
    out.extend_from_slice(&EMBED_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&payload_offset.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&sha256_bytes(payload));
    out
}

fn parse_trailer(trailer: &[u8; TRAILER_LEN]) -> Option<(u64, u64, [u8; 32])> {
    let mut r = Reader(trailer);
    if r.take(8)? != EMBED_MAGIC {
        return None;
    }
    if r.u32()? != EMBED_FORMAT_VERSION {
        return None;
    }
    let offset = r.u64()?;
    let len = r.u64()?;
    let sha: [u8; 32] = r.take(32)?.try_into().ok()?;
    Some((offset, len, sha))
}

fn parse_payload(payload: &[u8]) -> Option<EmbeddedSet> {
    let mut r = Reader(payload);
    let count = r.u32()?;
    let mut libraries = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let use_len = r.u32()? as usize;
        let use_ = std::str::from_utf8(r.take(use_len)?).ok()?.to_string();
        let source_sha256: [u8; 32] = r.take(32)?.try_into().ok()?;
        let cwasm_len = r.u64()? as usize;
        let artifact = r.take(cwasm_len)?.to_vec();
        libraries.insert(
            use_,
            EmbeddedLibrary {
                source_sha256,
                artifact,
            },
        );
    }
    if !r.rest().is_empty() {
        return None;
    }
    Some(EmbeddedSet { libraries })
}

// ---------------------------------------------------------------------------

/// A little-endian cursor over a byte slice; every read bounds-checked.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn rest(&self) -> &'a [u8] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid component (wasmtime's default `wat` feature accepts
    /// the text form) — big enough to exercise the burn pipeline without
    /// needing the SDK fixtures.
    const TINY: &[u8] = b"(component)";

    fn tiny_burn() -> (Vec<u8>, [u8; 32]) {
        let artifact = burn_component(TINY).expect("precompile a tiny component");
        (artifact, sha256_bytes(TINY))
    }

    #[test]
    fn burn_roundtrip_loads_a_component() {
        let (artifact, sha) = tiny_burn();
        assert!(artifact.starts_with(ARTIFACT_MAGIC));
        assert!(artifact_matches(&artifact, &sha));
        assert!(
            load_burned(&artifact, &sha).is_some(),
            "a well-formed artifact deserializes"
        );
    }

    #[test]
    fn header_mismatches_are_rejected() {
        let (artifact, sha) = tiny_burn();

        // Source digest mismatch (artifact burned from other bytes).
        let other = sha256_bytes(b"(component (another))");
        assert!(!artifact_matches(&artifact, &other));
        assert!(load_burned(&artifact, &other).is_none());

        // Magic.
        let mut bad = artifact.clone();
        bad[0] ^= 0xFF;
        assert!(load_burned(&bad, &sha).is_none());

        // Format version.
        let mut bad = artifact.clone();
        bad[8] ^= 0xFF;
        assert!(load_burned(&bad, &sha).is_none());

        // Engine tag: locate it right after the two length-prefixed strings.
        let mut off = 8 + 4;
        for _ in 0..2 {
            let len = u32::from_le_bytes(artifact[off..off + 4].try_into().unwrap()) as usize;
            off += 4 + len;
        }
        let mut bad = artifact.clone();
        bad[off] ^= 0xFF;
        assert!(load_burned(&bad, &sha).is_none());

        // Truncations and garbage.
        for cut in [8, 20, artifact.len() - 1] {
            assert!(load_burned(&artifact[..cut], &sha).is_none(), "cut at {cut}");
        }
        assert!(load_burned(b"PFSBURN1", &sha).is_none());
        assert!(load_burned(&[], &sha).is_none());
    }

    #[test]
    fn corrupted_payload_is_rejected_not_ub() {
        let (artifact, sha) = tiny_burn();
        // Corrupt the first payload byte (right after the fixed header fields)
        // — wasmtime's own artifact header lives there and must reject it.
        let mut off = 8 + 4;
        for _ in 0..2 {
            let len = u32::from_le_bytes(artifact[off..off + 4].try_into().unwrap()) as usize;
            off += 4 + len;
        }
        off += 8 + 32; // engine_tag + source sha256
        let mut bad = artifact.clone();
        bad[off] ^= 0xFF;
        // Header still matches; the payload itself is damaged. wasmtime must
        // refuse it — and either way the caller sees `None`, never UB.
        assert!(load_burned(&bad, &sha).is_none());
    }

    #[test]
    fn embedded_payload_roundtrip() {
        let libs = vec![
            (
                "/abs/dir/hello.wasm".to_string(),
                EmbeddedLibrary {
                    source_sha256: [7; 32],
                    artifact: b"cwasm-one".to_vec(),
                },
            ),
            (
                "/cache/libraries/deadbeef.wasm".to_string(),
                EmbeddedLibrary {
                    source_sha256: [9; 32],
                    artifact: b"cwasm-two".to_vec(),
                },
            ),
        ];
        let payload = encode_embedded_payload(&libs);
        let set = parse_payload(&payload).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set.get("/abs/dir/hello.wasm").unwrap().source_sha256, [7; 32]);
        assert_eq!(
            set.get("/cache/libraries/deadbeef.wasm").unwrap().artifact,
            b"cwasm-two"
        );
        assert!(set.get("/other.wasm").is_none());

        // Trailing garbage / truncation fail closed.
        let mut padded = payload.clone();
        padded.push(0);
        assert!(parse_payload(&padded).is_none());
        assert!(parse_payload(&payload[..payload.len() - 1]).is_none());
    }

    #[test]
    fn trailer_roundtrip() {
        let payload = encode_embedded_payload(&[]);
        let trailer = encode_embedded_trailer(&payload, 1234);
        assert_eq!(trailer.len(), TRAILER_LEN);
        let trailer: [u8; TRAILER_LEN] = trailer.try_into().unwrap();
        let (offset, len, sha) = parse_trailer(&trailer).unwrap();
        assert_eq!((offset, len), (1234, payload.len() as u64));
        assert_eq!(sha, sha256_bytes(&payload));

        let mut bad = trailer;
        bad[0] ^= 0xFF; // magic
        assert!(parse_trailer(&bad).is_none());
        let mut bad = trailer;
        bad[8] ^= 0xFF; // version
        assert!(parse_trailer(&bad).is_none());
    }
}
