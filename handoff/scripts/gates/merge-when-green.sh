#!/usr/bin/env bash
# Merge PR $1 only when every check has finished: no failure and nothing pending.
set -u
pr=$1 repo=ncecere/open-model-gateway
cd "$(dirname "$0")/.."
sleep 45
while true; do
  states=$(gh pr checks "$pr" --repo "$repo" 2>/dev/null | awk -F'\t' '{print $2}')
  summary=$(echo "$states" | sort | uniq -c | tr '\n' ' ')
  if echo "$states" | grep -qx fail; then echo "PR $pr FAILED: $summary"; exit 1; fi
  if [ -n "$states" ] && ! echo "$states" | grep -qx pending; then break; fi
  sleep 30
done
gh pr merge "$pr" --repo "$repo" --merge 2>&1 | tail -1
echo "PR $pr: $(gh pr view "$pr" --repo "$repo" --json state --jq .state) $summary"
