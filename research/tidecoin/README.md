# Tidecoin chain timing research

Read [the block-timing report](../../docs/TIDECOIN_BLOCK_TIMING.md).

`chain_timing.py` parses the local mainnet block files, links headers by previous
hash, selects the maximum-work chain and reports interval, retarget-window,
time-of-day and per-year statistics. The block files are read-only inputs; only
the requested JSON output is written.

```sh
python3 research/tidecoin/chain_timing.py --blocks "$TIDECOIN_BLOCKS" \
  --output research/tidecoin/results/chain-timing.json
```

Set `TIDECOIN_BLOCKS` to the directory containing the snapshot's `blk*.dat` files.

Requirements: Python 3 and a local a Tidecoin mainnet block snapshot snapshot (override
with `--blocks`). Peak memory is roughly 1 GiB for the full 1.85M-block chain.
`--max-files N` limits the scan to the first N block files for quick tests.

The parser assumes the mainnet message magic and the standard Bitcoin block-file
record layout. It measures timing only: proof-of-work validity, chainstate and
transaction contents are not checked. The saved `results/chain-timing.json`
records source-independent statistics only (no headers or addresses).
