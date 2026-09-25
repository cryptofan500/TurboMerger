#!/usr/bin/env bash
# A.1 / N-06 / N-31 — v7.7.0: merged=2 scan_skipped=120, exit 0 (every PDF dropped).
# Fixed (Phase 1): every PDF is listed as unsupported in the report, exit 3.
# Phase 3 adds PDF extraction. Usage: a01_arxiv_exit_code.sh <arxiv_corpus_dir>
. "$(dirname "$0")/common.sh"
set +e
"$TM" merge "${1:?corpus dir}" "$SP/arxiv_out.md"; echo "exit=$?"
