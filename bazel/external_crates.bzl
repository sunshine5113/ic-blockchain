"""
This module declares all direct rust dependencies.
"""

load("@rules_rust//crate_universe:defs.bzl", "crate", "crates_repository", "splicing_config")

def external_crates_repository(name):
    crates_repository(
        name = name,
        annotations = {
            "miracl_core_bls12381": [crate.annotation(
                rustc_flags = [
                    "-C",
                    "opt-level=3",
                ],
            )],
            "pprof": [crate.annotation(
                build_script_data = [
                    "@com_google_protobuf//:protoc",
                ],
                build_script_env = {
                    "PROTOC": "$(execpath @com_google_protobuf//:protoc)",
                },
            )],
            "prost-build": [crate.annotation(
                build_script_env = {
                    "PROTOC_NO_VENDOR": "1",
                    "PROTOC": "NO_PROTOC_PATH_AT_COMPILE_TIME",
                },
            )],
        },
        isolated = True,
        lockfile = "//:Cargo.Bazel.lock",
        packages = {
            # Keep sorted.
            "anyhow": crate.spec(version = "=1.0.31"),
            "arrayvec": crate.spec(version = "=0.5.1"),
            "askama": crate.spec(
                features = ["serde-json"],
                version = "=0.11.1",
            ),
            "assert_matches": crate.spec(version = "=1.3.0"),
            "async-stream": crate.spec(version = "=0.3.3"),
            "async-trait": crate.spec(version = "=0.1.53"),
            "backoff": crate.spec(version = "=0.3.0"),
            "base32": crate.spec(version = "=0.4.0"),
            "base64": crate.spec(version = "=0.11.0"),
            "bincode": crate.spec(version = "=1.2.1"),
            "bindgen": crate.spec(version = "=0.59.2"),
            "bitcoin": crate.spec(
                features = ["rand"],
                version = "=0.28.1",
            ),
            "bitflags": crate.spec(version = "=1.3.2"),
            "bit-vec": crate.spec(version = "=0.5"),
            "byte-unit": crate.spec(version = "=1.0.1"),
            "byteorder": crate.spec(version = "=1.4.3"),
            "bytes": crate.spec(version = "=1.0.1"),
            "bls12_381": crate.spec(
                default_features = False,
                features = [
                    "groups",
                    "pairings",
                    "alloc",
                    "experimental",
                ],
                version = "=0.5.0",
            ),
            "candid": crate.spec(version = "=0.7.13"),
            "candid_derive": crate.spec(version = "=0.4.5"),
            "cc": crate.spec(version = "=1.0.73"),
            "cfg-if": crate.spec(version = "=0.1.10"),
            "chrono": crate.spec(version = "=0.4.19"),
            "clap": crate.spec(
                features = ["derive"],
                version = "=3.1.6",
            ),
            "comparable": crate.spec(
                features = ["derive"],
                version = "=0.5.0",
            ),
            "crc32fast": crate.spec(version = "=1.3.2"),
            "criterion": crate.spec(version = "=0.3.4"),
            "crossbeam-channel": crate.spec(version = "=0.5.0"),
            "csv": crate.spec(version = "=1.1.3"),
            "cvt": crate.spec(version = "=0.1.1"),
            "derive_more": crate.spec(
                git = "https://github.com/dfinity-lab/derive_more",
                rev = "9f1b894e6fde640da4e9ea71a8fc0e4dd98d01da",
            ),
            "debug_stub_derive": crate.spec(version = "=0.3.0"),
            "ed25519-dalek": crate.spec(version = "=1.0.1"),
            "erased-serde": crate.spec(version = "=0.3.11"),
            "eyre": crate.spec(version = "=0.6.8"),
            "features": crate.spec(version = "=0.10.0"),
            "ff": crate.spec(
                default_features = False,
                features = ["std"],
                version = "=0.10.0",
            ),
            "flate2": crate.spec(version = "=1.0.20"),
            "float-cmp": crate.spec(version = "=0.9.0"),
            "futures-util": crate.spec(version = "=0.3.21"),
            "futures": crate.spec(version = "=0.3.21"),
            "getrandom": crate.spec(
                package = "getrandom",
                version = "=0.2.6",
            ),
            "hashlink": crate.spec(version = "=0.8.0"),
            "hex": crate.spec(version = "=0.4.3"),
            "hex-literal": crate.spec(version = "=0.2.1"),
            "http": crate.spec(version = "=0.2.5"),
            "hyper": crate.spec(
                features = [
                    "client",
                    "tcp",
                    "http1",
                    "http2",
                ],
                version = "=0.14.18",
            ),
            "hyper-tls": crate.spec(version = "=0.5.0"),
            "itertools": crate.spec(version = "=0.10.3"),
            "json5": crate.spec(version = "=0.4.1"),
            "k256": crate.spec(
                default_features = False,
                features = ["arithmetic"],
                version = "=0.10.3",
            ),
            "lazy_static": crate.spec(version = "=1.4.0"),
            "leb128": crate.spec(version = "=0.2.4"),
            "libc": crate.spec(version = "=0.2.126"),
            "libsecp256k1": crate.spec(version = "=0.5.0"),
            "linked-hash-map": crate.spec(version = "0.5.3"),
            "lru": crate.spec(
                default_features = False,
                version = "0.7.1",
            ),
            "maplit": crate.spec(version = "=1.0.2"),
            "miracl_core_bls12381": crate.spec(version = "=4.1.2"),
            "native-tls": crate.spec(
                features = ["alpn"],
                version = "=0.2.7",
            ),
            "nix": crate.spec(version = "=0.23.0"),
            "num-bigint-dig": crate.spec(
                features = ["prime"],
                version = "=0.8",
            ),
            "num-traits": crate.spec(version = "=0.2.12"),
            "once_cell": crate.spec(version = "=1.10.0"),
            "openssl": crate.spec(version = "=0.10.38"),
            "openssl-sys": crate.spec(version = "=0.9.70"),
            "p256": crate.spec(
                default_features = False,
                features = ["arithmetic"],
                version = "=0.10",
            ),
            "pairing": crate.spec(version = "=0.20.0"),
            "parking_lot": crate.spec(version = "0.11.1"),
            "paste": crate.spec(version = "=1.0.0"),
            "pathdiff": crate.spec(version = "=0.2.1"),
            "pkg-config": crate.spec(version = "=0.3.25"),
            "pprof": crate.spec(
                default_features = False,
                features = [
                    "backtrace-rs",
                    "flamegraph",
                    "prost-codec",
                ],
                version = "=0.9.1",
            ),
            "proc-macro2": crate.spec(version = "=1.0.36"),
            "procfs": crate.spec(
                default_features = False,
                version = "=0.9.1",
            ),
            "prometheus": crate.spec(
                features = ["process"],
                version = "=0.13.0",
            ),
            "proptest": crate.spec(version = "=0.9.6"),
            "proptest-derive": crate.spec(version = "=0.1.2"),
            "prost": crate.spec(version = "=0.10.4"),
            "prost-build": crate.spec(version = "=0.10.4"),
            "quote": crate.spec(version = "=1.0.17"),
            "rand-0_7_3": crate.spec(
                package = "rand",
                version = "=0.7.3",
            ),
            "rand-0_8_4": crate.spec(
                package = "rand",
                version = "=0.8.4",
            ),
            "rand_chacha": crate.spec(version = "=0.2.2"),
            "rand_chacha-0_3_1": crate.spec(
                package = "rand_chacha",
                version = "=0.3.1",
            ),
            "rand_core": crate.spec(version = "=0.5.1"),
            "regex": crate.spec(version = "=1.5.6"),
            "rustversion": crate.spec(version = "=1.0.2"),
            "scoped_threadpool": crate.spec(version = "=0.1.0"),
            "serde_bytes": crate.spec(version = "=0.11.6"),
            "serde_cbor": crate.spec(version = "=0.11.1"),
            "serde_derive": crate.spec(version = "=1.0.136"),
            "serde_json": crate.spec(version = "=1.0.40"),
            "serde_with": crate.spec(version = "=1.6.2"),
            "serde": crate.spec(
                features = ["derive"],
                version = "=1.0.136",
            ),
            "sha2": crate.spec(version = "=0.9.9"),
            "simple_asn1": crate.spec(version = "=0.5.4"),
            "slog": crate.spec(
                features = [
                    "nested-values",
                    "max_level_trace",
                    "release_max_level_debug",
                ],
                version = "=2.5.2",
            ),
            "slog-async": crate.spec(
                features = ["nested-values"],
                version = "=2.7.0",
            ),
            "slog-json": crate.spec(
                features = ["nested-values"],
                version = "=2.3.0",
            ),
            "slog-scope": crate.spec(version = "=4.1.2"),
            "slog-term": crate.spec(version = "=2.6.0"),
            "slog_derive": crate.spec(version = "=0.2.0"),
            "strum": crate.spec(version = "=0.23.0"),
            "strum_macros": crate.spec(version = "=0.23.0"),
            "subtle": crate.spec(version = "=2.4"),
            "syn": crate.spec(
                features = [
                    "fold",
                    "full",
                ],
                version = "=1.0.93",
            ),
            "tar": crate.spec(version = "=0.4.38"),
            "tempfile": crate.spec(version = "=3.1.0"),
            "thiserror": crate.spec(version = "=1.0.30"),
            "threadpool": crate.spec(version = "1.8.1"),
            "toml": crate.spec(version = "=0.5.9"),
            "tokio": crate.spec(
                features = ["full"],
                version = "=1.18.1",
            ),
            "tonic": crate.spec(version = "=0.7.2"),
            "tonic-build": crate.spec(version = "=0.7.2"),
            "tower": crate.spec(version = "=0.4.11"),
            "url": crate.spec(
                features = ["serde"],
                version = "=2.2.1",
            ),
            "uuid": crate.spec(
                features = ["v4"],
                version = "=0.8.1",
            ),
            "wsl": crate.spec(version = "=0.1.0"),
            "wycheproof": crate.spec(version = "=0.4.0"),
            "zeroize": crate.spec(
                features = ["zeroize_derive"],
                version = "=1.4.3",
            ),
        },
        splicing_config = splicing_config(
            resolver_version = "2",
        ),
    )
