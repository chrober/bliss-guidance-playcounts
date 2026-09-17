# bliss-guidance-playcounts

`bliss-guidance-playcounts` is a provider addon for the
`bliss-playlist-optimizer` guidance SPI. It reads an `lms-play-counts-v1` raw
snapshot, maps counts to stable candidate identities, and returns bounded
global preference guidance for each requested candidate. The optimizer applies
the job's signed play-count influence; this addon does not decide hard
eligibility or route validity.

Its SPI provider ID is `playcount-guidance`. It reads a frozen snapshot rather
than querying LMS directly, so addon execution remains deterministic and
network-free.

The addon communicates through versioned JSONL on stdin/stdout. It is intended
to be discovered and started by the optimizer, not called directly by LMS.

Prepare options:

```json
{"artifact_path":"/path/to/play-counts.json"}
```

Unknown counts are treated as zero for percentile ordering, matching the
optimizer's current play-count contract. The provider remains network-free and
does not access the LMS database directly.
