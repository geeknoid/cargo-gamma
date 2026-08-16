#!/usr/bin/env bash
#
# Surface flaky tests from a nextest JUnit report.
#
# A flaky test — one that failed and then passed on retry — is reported by nextest as FLAKY, but
# the process still exits zero, so without this step a flake is indistinguishable from a clean
# pass in the CI status. That is the state this script exists to prevent: an intermittently red
# test that nobody is accountable for, which trains people to re-run the job.
#
# The script fails the step when a flake is seen. That is deliberate, and the escape hatch is not
# to re-run: it is to fix the test, or to quarantine it explicitly in `.config/nextest.toml` with
# a reason and a tracking item, which makes the decision visible in review. See `docs/DESIGN.md`,
# "Flaky tests".

set -euo pipefail

report="${1:-target/nextest/ci/junit.xml}"

if [[ ! -f "${report}" ]]; then
    echo "No JUnit report at ${report}; nothing to check for flakes."
    exit 0
fi

# Nextest records each failed-then-passed attempt as a <flakyFailure> or <flakyError> child
# of the <testcase> that eventually passed. JUnit output is minified, so count elements rather
# than physical lines.
flaky_count=$(awk '
    {
        line = $0
        while (match(line, /<flaky(Failure|Error)([[:space:]\/>])/)) {
            count++
            line = substr(line, RSTART + RLENGTH)
        }
    }
    END { print count + 0 }
' "${report}")

if [[ "${flaky_count}" -eq 0 ]]; then
    echo "No flaky tests detected."
    exit 0
fi

# Pull out the name/classname of every testcase that contains a flaky result. Attribute order is
# not guaranteed, so each is matched independently rather than by one positional pattern. Kept to
# awk and sed so the script has no dependency beyond what every runner image already has.
extract() {
    sed -E "s/.*[[:space:]]$1=\"([^\"]*)\".*/\1/;t;d"
}

names=$(tr '>' '>\n' <"${report}" |
    awk '
        /<testcase([[:space:]>])/ { testcase = $0 }
        /<flaky(Failure|Error)([[:space:]\/>])/ && testcase != "" { print testcase }
        /<\/testcase>/ { testcase = "" }
    ' |
    while IFS= read -r line; do
        class=$(printf '%s' "${line}" | extract classname)
        name=$(printf '%s' "${line}" | extract name)
        printf '%s::%s\n' "${class:-unknown}" "${name:-unknown}"
    done |
    sort -u || true)

echo "::group::Flaky tests"
echo "${names}"
echo "::endgroup::"

while IFS= read -r name; do
    [[ -z "${name}" ]] && continue
    echo "::warning title=Flaky test::${name} failed and then passed on retry"
done <<<"${names}"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "### Flaky tests"
        echo
        echo "${flaky_count} retried attempt(s) across the following tests:"
        echo
        while IFS= read -r name; do
            [[ -z "${name}" ]] && continue
            echo "- \`${name}\`"
        done <<<"${names}"
        echo
        echo "A flaky test is not a passing test. Fix it, or quarantine it explicitly in"
        echo "\`.config/nextest.toml\` with a reason — see \`docs/DESIGN.md\`, \"Flaky tests\"."
    } >>"${GITHUB_STEP_SUMMARY}"
fi

echo "Failing the step: a flaky result must not be folded into a pass."
exit 1
