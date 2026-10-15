# smb-rs 0.12.1, patched for FramePlayer

Upstream: https://crates.io/crates/smb/0.12.1 (https://github.com/afiffon/smb-rs),
MIT licence. Used through `[patch.crates-io]` in the workspace `Cargo.toml`;
the upstream tests are left out. Two changes, both marked "FramePlayer
patch" in the source:

1. `src/connection/worker/worker_trait.rs`: credits granted in interim
   (STATUS_PENDING) responses are counted. Servers grant an asynchronous
   request's credits in the interim response and none in the final one
   (Samba sets 0), but only the final response reached the credit
   accounting. A NAS answers a read that waits on its disk with an interim
   response, so each such read (most reads after a seek) lost its credits
   until every request blocked: playback from an SMB share stopped for good
   after a few seeks, and nothing opened from the share until a restart.
2. `src/sync_helpers.rs`: waiting for credits gives up after 30 s with an
   error instead of blocking forever, so FramePlayer can drop the connection
   and reconnect.

To update smb-rs: copy the new release here (without `tests/`, the
`[[test]]` and `[dev-dependencies]` sections of its `Cargo.toml`), and carry
the changes over unless upstream has fixed them.
