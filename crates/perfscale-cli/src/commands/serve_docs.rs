//! Embedded documentation for the `perfscale serve` dashboard.
//!
//! The dashboard (`serve_ui.html`) renders the markdown docs from `docs/`
//! (English and the Russian mirror in `docs/ru/`) inside a "Docs" tab, so the
//! served page stays a single self-contained HTML file — no CDN, no build
//! step for the consumer.
//!
//! All rendering lives behind `#[cfg(test)]`: `pulldown-cmark` is a
//! dev-dependency only and the CLI binary carries no markdown machinery. The
//! test [`tests::docs_data_html_is_up_to_date`] reads `docs/` from the repo
//! checkout (only at test time), regenerates [`serve_docs_data.html`] — a
//! `<script>window.PERFSCALE_DOCS = …</script>` blob committed next to this
//! file — and fails if the two drift apart. Run
//! `PERFSCALE_UPDATE_DOCS=1 cargo test -p perfscale-cli serve_docs` to
//! regenerate after editing the docs.
//!
//! Page format (JSON): an array of
//! `{ slug, title, group, html_en, html_ru }` where `slug` is the doc path
//! without `.md` (`core/actions`), `group` is `top` | `cli` | `core`, and
//! `html_ru` is `null` when the Russian mirror has no counterpart. Relative
//! `*.md` links become `#/docs/<slug>` hash routes, external links open in a
//! new tab, and relative images are inlined as base64 data URIs.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use pulldown_cmark::{Event, Options, Parser, Tag};

    /// `crates/perfscale-cli` → repo root `docs/`.
    fn docs_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("docs")
    }

    /// The committed blob this test keeps in sync, next to this source file.
    fn data_file() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/serve_docs_data.html")
    }

    struct Page {
        slug: String,
        title: String,
        group: String,
        html_en: String,
        html_ru: Option<String>,
    }

    /// Every English markdown page under `docs/` (skipping `ru/` and
    /// `images/`), sorted by slug for a stable output.
    fn collect_pages() -> Vec<(String, PathBuf)> {
        let mut pages = Vec::new();
        let root = docs_dir();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path == root.join("ru") || path == root.join("images") {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "md") {
                    let rel = path.strip_prefix(&root).unwrap().with_extension("");
                    let slug = rel.to_str().unwrap().replace('\\', "/");
                    pages.push((slug, path));
                }
            }
        }
        pages.sort_by(|a, b| a.0.cmp(&b.0));
        pages
    }

    /// Title from the first `# …` heading; falls back to the slug.
    fn title_of(markdown: &str, slug: &str) -> String {
        for line in markdown.lines() {
            if let Some(rest) = line.strip_prefix("# ") {
                return rest.trim().to_string();
            }
        }
        slug.to_string()
    }

    /// Normalize `base_dir.join(href)` into a docs-relative path, resolving
    /// `..` segments. Returns `None` if it escapes the docs root.
    fn resolve_rel(base_dir: &Path, href: &str) -> Option<PathBuf> {
        let mut out: Vec<String> = base_dir
            .components()
            .map(|c| c.as_os_str().to_str().unwrap().to_string())
            .collect();
        for part in href.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    out.pop()?;
                }
                p => out.push(p.to_string()),
            }
        }
        Some(out.iter().collect())
    }

    /// Hand-rolled base64 (standard alphabet, with padding) so the generator
    /// needs no extra dependency for inlining images.
    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// Rewrite one link/image destination relative to `base_dir`:
    /// `*.md` links → `#/docs/<slug>` (anchors dropped), relative images →
    /// base64 data URIs, everything else (http(s), mailto) untouched.
    fn rewrite_dest(base_dir: &Path, dest: &str, image: bool) -> String {
        if dest.starts_with("http://") || dest.starts_with("https://") || dest.contains(':') {
            return dest.to_string();
        }
        let path_part = dest.split('#').next().unwrap();
        let Some(resolved) = resolve_rel(base_dir, path_part) else {
            return dest.to_string();
        };
        if image {
            let root = docs_dir();
            let full = root.join(&resolved);
            let bytes = std::fs::read(&full).expect("image referenced by docs must exist");
            let mime = match full.extension().and_then(|e| e.to_str()) {
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("svg") => "image/svg+xml",
                Some("gif") => "image/gif",
                _ => "application/octet-stream",
            };
            return format!("data:{mime};base64,{}", base64_encode(&bytes));
        }
        if let Some(no_ext) = resolved.to_str().unwrap().strip_suffix(".md") {
            return format!("#/docs/{}", no_ext.replace('\\', "/"));
        }
        dest.to_string()
    }

    /// Render markdown to HTML with tables and strikethrough, rewriting links
    /// and images per [`rewrite_dest`], and making external links open in a
    /// new tab. `base_dir` is the directory of the source file relative to
    /// the language's docs root (`""`, `cli`, `core`).
    fn render(markdown: &str, base_dir: &Path) -> String {
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
        let parser = Parser::new_ext(markdown, options).map(|event| match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Link {
                link_type,
                dest_url: rewrite_dest(base_dir, &dest_url, false).into(),
                title,
                id,
            }),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Image {
                link_type,
                dest_url: rewrite_dest(base_dir, &dest_url, true).into(),
                title,
                id,
            }),
            other => other,
        });
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, parser);
        // pulldown-cmark emits plain <a href="…">: the only http(s) hrefs left
        // are external ones (relative .md links were rewritten above).
        html.replace("<a href=\"http", "<a target=\"_blank\" rel=\"noopener\" href=\"http")
    }

    fn generate() -> String {
        let root = docs_dir();
        let ru_root = root.join("ru");
        let mut pages: Vec<Page> = Vec::new();
        for (slug, en_path) in collect_pages() {
            let en_md = std::fs::read_to_string(&en_path).unwrap();
            let base_dir = en_path.parent().unwrap().strip_prefix(&root).unwrap();
            let ru_path = ru_root.join(en_path.strip_prefix(&root).unwrap());
            let html_ru = ru_path.exists().then(|| {
                let ru_md = std::fs::read_to_string(&ru_path).unwrap();
                render(&ru_md, base_dir)
            });
            let group = slug.split('/').next().unwrap();
            pages.push(Page {
                title: title_of(&en_md, &slug),
                group: match group {
                    "cli" | "core" => group.to_string(),
                    _ => "top".to_string(),
                },
                html_en: render(&en_md, base_dir),
                html_ru,
                slug,
            });
        }
        let json = serde_json::to_string(
            &pages
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "slug": p.slug,
                        "title": p.title,
                        "group": p.group,
                        "html_en": p.html_en,
                        "html_ru": p.html_ru,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
        // `<\/` stays valid JSON and can never close the <script> early.
        .replace("</", "<\\/");
        format!("<script>window.PERFSCALE_DOCS = {json};</script>\n")
    }

    /// Committed copy of the generated blob — drift between `docs/` and this
    /// file fails CI; regenerate with `PERFSCALE_UPDATE_DOCS=1`.
    #[test]
    fn docs_data_html_is_up_to_date() {
        let generated = generate();
        if std::env::var_os("PERFSCALE_UPDATE_DOCS").is_some() {
            std::fs::write(data_file(), &generated).unwrap();
            return;
        }
        let committed = include_str!("serve_docs_data.html");
        assert_eq!(
            generated, committed,
            "docs data is stale — regenerate with `PERFSCALE_UPDATE_DOCS=1 \
             cargo test -p perfscale-cli serve_docs`"
        );
    }

    #[test]
    fn base64_round_trips_standard_alphabet() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn render_rewrites_links_and_images() {
        let base = Path::new("cli");
        let html = render(
            "see [actions](../core/actions.md#stdthresholdsv1), [k6](https://k6.io), \
             [ref](commands.md)\n\n![img](../images/serve-dashboard.png)\n",
            base,
        );
        assert!(html.contains("href=\"#/docs/core/actions\""), "{html}");
        assert!(html.contains("href=\"#/docs/cli/commands\""), "{html}");
        assert!(
            html.contains("target=\"_blank\" rel=\"noopener\" href=\"https://k6.io\""),
            "{html}"
        );
        assert!(html.contains("src=\"data:image/png;base64,"), "{html}");
        assert!(!html.contains("src=\"http"), "{html}");
    }
}
