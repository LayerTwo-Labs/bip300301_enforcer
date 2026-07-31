#!/usr/bin/env python3
"""Regenerate the assumeutxo integration-test fixture for one Bitcoin Core major.

The `assumeutxo` integration test needs a UTXO snapshot that Bitcoin Core
will accept via `loadtxoutset` on regtest. Core only accepts snapshots whose
base block hash is hardcoded in the regtest chainparams (`m_assumeutxo_data`),
and those entries correspond to the deterministic chain built by Core's own
functional-test framework in `feature_assumeutxo.py` — a chain that differs
between Core major versions (it changed between v29 and v30, for example).

This script replays that exact chain-generation sequence using the
functional-test framework of a specific Bitcoin Core source tree, driving the
matching bitcoind, then writes:

  blocks-v<majors>.hex   - all 299 raw blocks, one hex-encoded block per line
                           (line N = block at height N; a serialized header is
                           the first 80 bytes of its block)
  fixtures.json          - manifest mapping Core major versions to their
                           blocks file and expected snapshot hashes

The generated chain is verified against `CHAINPARAMS_ASSUMEUTXO_299` below
(transcribed from each major's regtest chainparams), so a silent mismatch
(e.g. after a Core upgrade changes the deterministic test chain) fails
loudly. Majors that share identical chainparams entries (30 and 31, say)
share one blocks file and one manifest entry.

The snapshot file itself is deliberately not written: the integration test
derives it from the blocks file with `dumptxoutset` and caches it, so no
binary blob needs to be committed.

Usage (from the repo root, with .integration-deps in place):

    # v29 fixture, via the patched node
    python3 scripts/generate_assumeutxo_fixture.py

    # v30/v31 fixture, via a stock source tree + binaries
    python3 scripts/generate_assumeutxo_fixture.py \\
        --bitcoin-repo /path/to/bitcoin-31.0-src \\
        --bitcoin-bins .integration-deps/bitcoin-stock-31.0

When adding support for a new Core major: add its regtest chainparams entry
at height 299 to `CHAINPARAMS_ASSUMEUTXO_299`, download that version's
source tree, and rerun this script with it.
"""

import argparse
import json
import os
import sys
import tempfile

START_HEIGHT = 199
SNAPSHOT_BASE_HEIGHT = 299

# The regtest `m_assumeutxo_data` entry at height 299, per Core major
# version, from src/kernel/chainparams.cpp of each release.
CHAINPARAMS_ASSUMEUTXO_299 = {
    29: {
        "blockhash": "3bb7ce5eba0be48939b7a521ac1ba9316afee2c7bada3a0cca24188e6d7d96c0",
        "txoutset_hash": "a4bf3407ccb2cc0145c49ebba8fa91199f8a3903daf0883875941497d2493c27",
        "nchaintx": 334,
    },
    30: {
        "blockhash": "7cc695046fec709f8c9394b6f928f81e81fd3ac20977bb68760fa1faa7916ea2",
        "txoutset_hash": "d2b051ff5e8eef46520350776f4100dd710a63447a8e01d917e92e79751a63e2",
        "nchaintx": 334,
    },
    31: {
        "blockhash": "7cc695046fec709f8c9394b6f928f81e81fd3ac20977bb68760fa1faa7916ea2",
        "txoutset_hash": "d2b051ff5e8eef46520350776f4100dd710a63447a8e01d917e92e79751a63e2",
        "nchaintx": 334,
    },
}

CONFIG_INI_TEMPLATE = """\
[environment]
CLIENT_NAME=Bitcoin Core
CLIENT_BUGREPORT=https://github.com/bitcoin/bitcoin/issues
SRCDIR={srcdir}
BUILDDIR={builddir}
EXEEXT=
RPCAUTH={srcdir}/share/rpcauth/rpcauth.py

[components]
ENABLE_WALLET=true
USE_SQLITE=true
ENABLE_CLI=true
ENABLE_BITCOIN_UTIL=true
ENABLE_WALLET_TOOL=true
ENABLE_BITCOIND=true
ENABLE_ZMQ=true
"""


def parse_args():
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bitcoin-repo",
        default=os.path.join(repo_root, ".integration-deps", "bitcoin-patched-repo"),
        help="Bitcoin Core source tree providing the functional-test framework",
    )
    parser.add_argument(
        "--bitcoin-bins",
        default=os.path.join(repo_root, ".integration-deps", "bitcoin-patched-latest"),
        help="Directory holding the matching bitcoind/bitcoin-cli/bitcoin-util binaries",
    )
    parser.add_argument(
        "--out-dir",
        default=os.path.join(repo_root, "integration_tests", "testdata", "assumeutxo"),
        help="Fixture output directory",
    )
    return parser.parse_args()


def update_manifest(out_dir, majors, blocks_filename, expected):
    """Merge an entry for `majors` into fixtures.json, replacing any entry
    that covers one of these majors."""
    manifest_path = os.path.join(out_dir, "fixtures.json")
    entries = []
    if os.path.exists(manifest_path):
        with open(manifest_path) as f:
            entries = json.load(f)
    entries = [e for e in entries if not set(e["majors"]) & set(majors)]
    entries.append(
        {
            "majors": majors,
            "blocks": blocks_filename,
            "base_height": SNAPSHOT_BASE_HEIGHT,
            "base_blockhash": expected["blockhash"],
            "txoutset_hash": expected["txoutset_hash"],
        }
    )
    entries.sort(key=lambda e: e["majors"])
    with open(manifest_path, "w") as f:
        json.dump(entries, f, indent=2)
        f.write("\n")


def main():
    args = parse_args()
    functional_dir = os.path.join(args.bitcoin_repo, "test", "functional")
    if not os.path.isdir(functional_dir):
        sys.exit(f"functional test framework not found at {functional_dir}")
    for binary in ("bitcoind", "bitcoin-cli", "bitcoin-util"):
        if not os.path.exists(os.path.join(args.bitcoin_bins, binary)):
            sys.exit(f"missing binary: {os.path.join(args.bitcoin_bins, binary)}")

    workdir = tempfile.mkdtemp(prefix="assumeutxo-fixture-")

    # The framework locates binaries at BUILDDIR/bin/<name>; fake that layout.
    fake_bin = os.path.join(workdir, "build", "bin")
    os.makedirs(fake_bin)
    for binary in ("bitcoind", "bitcoin-cli", "bitcoin-util"):
        os.symlink(os.path.join(args.bitcoin_bins, binary), os.path.join(fake_bin, binary))

    config_ini = os.path.join(workdir, "config.ini")
    with open(config_ini, "w") as f:
        f.write(
            CONFIG_INI_TEMPLATE.format(
                srcdir=os.path.abspath(args.bitcoin_repo),
                builddir=os.path.join(workdir, "build"),
            )
        )

    sys.path.insert(0, functional_dir)
    from test_framework.test_framework import BitcoinTestFramework
    from test_framework.wallet import MiniWallet

    out_dir = os.path.abspath(args.out_dir)

    class GenerateAssumeutxoFixture(BitcoinTestFramework):
        def set_test_params(self):
            # Build on the framework's pregenerated deterministic chain
            # (height 199), like feature_assumeutxo.py.
            self.num_nodes = 1

        def setup_network(self):
            # The default `setup_network` mines a fresh block on top of the
            # cached chain; feature_assumeutxo.py skips it the same way to
            # start exactly at height 199.
            self.add_nodes(self.num_nodes)
            self.start_nodes()

        def run_test(self):
            n0 = self.nodes[0]

            major = n0.getnetworkinfo()["version"] // 10_000
            expected = CHAINPARAMS_ASSUMEUTXO_299.get(major)
            if expected is None:
                raise AssertionError(
                    f"no chainparams entry known for Bitcoin Core major {major}; "
                    "add it to CHAINPARAMS_ASSUMEUTXO_299"
                )
            # Majors with identical chainparams entries share one fixture.
            majors = sorted(
                m for m, e in CHAINPARAMS_ASSUMEUTXO_299.items() if e == expected
            )

            mini_wallet = MiniWallet(n0)

            # Everything below must mirror feature_assumeutxo.py's run_test
            # exactly up to its `dumptxoutset` call: the chainparams
            # assumeutxo entry at height 299 commits to this exact chain.
            n0.setmocktime(n0.getblockheader(n0.getbestblockhash())["time"])

            assert n0.getblockcount() == START_HEIGHT
            for i in range(100):
                if i % 3 == 0:
                    mini_wallet.send_self_transfer(from_node=n0)
                self.generate(n0, nblocks=1, sync_fun=self.no_op)
                if i == 4:
                    # Stale block that forks off before the snapshot.
                    temp_invalid = n0.getbestblockhash()
                    n0.invalidateblock(temp_invalid)
                    stale_hash = self.generateblock(
                        n0, output="raw(aaaa)", transactions=[], sync_fun=self.no_op
                    )["hash"]
                    n0.invalidateblock(stale_hash)
                    n0.reconsiderblock(temp_invalid)

            assert n0.getblockcount() == SNAPSHOT_BASE_HEIGHT

            base_hash = n0.getbestblockhash()
            if base_hash != expected["blockhash"]:
                raise AssertionError(
                    f"generated chain tip {base_hash} does not match the "
                    f"chainparams assumeutxo entry {expected['blockhash']} for "
                    f"Core major {major}; the deterministic test chain has changed"
                )

            dump = n0.dumptxoutset("utxos.dat", "latest")
            if dump["txoutset_hash"] != expected["txoutset_hash"]:
                raise AssertionError(
                    f"txoutset hash {dump['txoutset_hash']} != expected "
                    f"{expected['txoutset_hash']}"
                )
            if dump["nchaintx"] != expected["nchaintx"]:
                raise AssertionError(
                    f"nchaintx {dump['nchaintx']} != expected {expected['nchaintx']}"
                )

            blocks_filename = "blocks-" + "-".join(f"v{m}" for m in majors) + ".hex"
            os.makedirs(out_dir, exist_ok=True)
            with open(os.path.join(out_dir, blocks_filename), "w") as f:
                for height in range(1, SNAPSHOT_BASE_HEIGHT + 1):
                    f.write(n0.getblock(n0.getblockhash(height), 0) + "\n")
            update_manifest(out_dir, majors, blocks_filename, expected)
            self.log.info(
                f"fixture for Core major(s) {majors} written to {out_dir}"
            )

    sys.argv = [
        sys.argv[0],
        f"--configfile={config_ini}",
        f"--tmpdir={os.path.join(workdir, 'tmp')}",
        f"--cachedir={os.path.join(workdir, 'cache')}",
    ]
    GenerateAssumeutxoFixture(__file__).main()


if __name__ == "__main__":
    main()
