//! `perfscale cache clean` — sweep the local import/library cache
//! (`~/.cache/perfscale`). Everything there is content-addressed and
//! re-fetchable, so eviction is always safe: the next `install`/`run`
//! rebuilds what a pinned `perfscale.lock` references.

use crate::cli::CacheCleanArgs;
use crate::error::CliError;

/// Human-readable byte count (KiB/MiB/GiB, one decimal).
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match bytes {
        b if b >= GIB => format!("{:.1} GiB", b as f64 / GIB as f64),
        b if b >= MIB => format!("{:.1} MiB", b as f64 / MIB as f64),
        b if b >= KIB => format!("{:.1} KiB", b as f64 / KIB as f64),
        b => format!("{b} B"),
    }
}

pub async fn run(args: CacheCleanArgs) -> Result<(), CliError> {
    let sweep = perfscale_core::import::sweep_cache(None, args.dry_run)
        .map_err(|e| CliError::new("failed to clean the cache").cause(e))?;

    if sweep.is_empty() {
        println!("cache is already empty — nothing to do");
        return Ok(());
    }
    let verb = if args.dry_run { "would free" } else { "freed" };
    for (area, entries, bytes) in &sweep.areas {
        println!("{area}: {entries} entries, {}", human_size(*bytes));
    }
    println!(
        "cache clean: {verb} {} across {} entries",
        human_size(sweep.total_bytes()),
        sweep.total_entries()
    );
    if args.dry_run {
        println!("(dry run — re-run without --dry-run to actually remove)");
    } else {
        println!("entries are re-fetched on demand from the pinned perfscale.lock digests");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_picks_sensible_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(43_900_000), "41.9 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
