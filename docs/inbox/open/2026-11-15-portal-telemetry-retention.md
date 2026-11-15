from: portal to: lattice raised: 2026-11-15 raised-by: Portal journey audit session (JOURNEY.md JD706) priority: normal status: open

Ask

Give `adoption_metrics.sqlite3` a retention policy. Prune or roll up `adoption_metric_events` so the repository storage home stays bounded, and count it in `lattice storage status` so the operator can see it.

Why

In the Portal repository home (`.lattice/`) the telemetry database had grown to 4.33 GB holding 22,136,871 rows in `adoption_metric_events` (page_size 4096, page_count 1,056,722, freelist 0, so it is live data, not slack). `lattice storage status` reported the telemetry class as 2.6 MB allocated, so the inventory under-reports it by three orders of magnitude, and `storage cache plan` does not cover it (it refused with "cache pressure is unknown until a fresh bounded inventory cycle completes" across several attempts on 2026-11-15). The owner asked for the Lattice cache to be removed to recover disk; the derived caches (checkout caches, history-object-cache.db, embedding-objects, symbol-bodies) came back 18 GB to 5.2 GB following docs/storage-lifecycle.md, but the telemetry file is not cache and docs name no safe reset for it, so it was left in place.

Done when

`lattice storage status --workspace /Users/pete/Cadres/portal` reports the telemetry database's real allocated bytes, and either a documented `lattice storage` verb or an automatic retention window keeps `adoption_metric_events` below a stated bound (for example 30 days or N rows) without losing the daily roll-ups in `capture_outcome_daily`.

Related

docs/telemetry-storage.md, docs/operator-storage.md, docs/storage-lifecycle.md ("To reset derived data"). Also seen the same day: `storage cache plan` never completed an inventory cycle while the daemon was serving four workspaces, so the plan/apply path could not be used at all and the reset had to be done by hand per the lifecycle doc.

Response

