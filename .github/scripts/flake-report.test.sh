#!/usr/bin/env bash

set -euo pipefail

root=$(mktemp -d)
trap 'rm -r "${root}"' EXIT

report="${root}/junit.xml"
summary="${root}/summary.md"
output="${root}/output.txt"

cat >"${report}" <<'XML'
<testsuites><testsuite><testcase classname="suite" name="errors then passes"><flakyError message="signal"/></testcase></testsuite></testsuites>
XML

if GITHUB_STEP_SUMMARY="${summary}" ./.github/scripts/flake-report.sh "${report}" >"${output}"; then
    echo "an error-kind flake passed the gate" >&2
    exit 1
fi

grep -F 'suite::errors then passes' "${output}" >/dev/null
grep -F '1 retried attempt(s)' "${summary}" >/dev/null

cat >"${report}" <<'XML'
<testsuites><testsuite><testcase name="several attempts" classname="suite"><flakyFailure/><flakyFailure/><flakyError/></testcase></testsuite></testsuites>
XML
: >"${summary}"

if GITHUB_STEP_SUMMARY="${summary}" ./.github/scripts/flake-report.sh "${report}" >"${output}"; then
    echo "multiple same-line flakes passed the gate" >&2
    exit 1
fi

grep -F 'suite::several attempts' "${output}" >/dev/null
grep -F '3 retried attempt(s)' "${summary}" >/dev/null

cat >"${report}" <<'XML'
<testsuites><testsuite><testcase classname="suite" name="clean"/></testsuite></testsuites>
XML

./.github/scripts/flake-report.sh "${report}" >"${output}"
grep -F 'No flaky tests detected.' "${output}" >/dev/null
