# Fuzzing (M5 hardening)

Coverage-guided ([cargo-fuzz] / libFuzzer) fuzzing of the untrusted-bytes
parsers — the bytes an attacker fully controls and that run *before* (or
adjacent to) authentication.

| Target          | Parser under test                                   | Crate            |
|-----------------|-----------------------------------------------------|------------------|
| `frame_decode`  | `decode_frame_bare` + `FrameDecoder` (THE realtime codec) | `dice-protocol`  |
| `voice_frame`   | `VoiceFrame::decode` (voice QUIC datagram)          | `dice-voice-core`|
| `history_query` | `parse_history_query` (`?before/after/limit` REST query) | `api-gateway` |

The contract each upholds: **no input may panic, hang, or allocate past the
frame cap** — only return an error. libFuzzer reports a crash on any panic.

## Running

cargo-fuzz needs a **nightly** toolchain + libFuzzer, so this runs on Linux or
macOS (not the Windows dev host). `protoc` must be on PATH (the `dice-protocol`
build), and `SQLX_OFFLINE=1` lets `api-gateway` compile against the committed
`.sqlx` cache without a database.

```sh
cargo install cargo-fuzz          # once
export SQLX_OFFLINE=1

cargo +nightly fuzz run frame_decode      # runs until a crash / Ctrl-C
cargo +nightly fuzz run voice_frame  -- -max_total_time=120
cargo +nightly fuzz run history_query
```

A crash writes the reproducing input under `fuzz/artifacts/<target>/`; replay it
with `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>` and,
once fixed, keep it as a regression seed in `fuzz/corpus/<target>/`.

## CI

`.github/workflows/fuzz.yml` runs each target for a bounded time on a weekly
schedule, on manual dispatch, and on PRs that touch a fuzzed parser or `fuzz/`.

[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz
