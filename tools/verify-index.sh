#!/usr/bin/env bash
# List every "[verify]" marker in the code base (unconfirmed Steam Frame
# platform assumptions) as a Markdown table, grouped by file.
#
#   tools/verify-index.sh                       print the table
#   tools/verify-index.sh --update docs/platform-notes.md
#       replace the section between <!-- BEGIN verify-index --> and
#       <!-- END verify-index --> in that file
set -euo pipefail
cd "$(dirname "$0")/.."

table() {
  echo "| Location | Assumption to check on hardware |"
  echo "|---|---|"
  # shellcheck disable=SC2016
  git ls-files -co --exclude-standard -- crates tools docker dist .github \
    | grep -E '\.(rs|sh|toml|yml|html)$|Dockerfile' | grep -v 'verify-index.sh' | sort \
    | while read -r f; do
        case "$f" in
          *.rs) cm='//[/!]?' ;;
          *.html) cm='<!--' ;;
          *) cm='#' ;;
        esac
        awk -v file="$f" -v cm="$cm" '
          function flush() {
            if (text != "") {
              gsub(/\|/, "\\|", text); gsub(/[ \t]+/, " ", text)
              if (length(text) > 260) text = substr(text, 1, 257) "..."
              printf "| `%s:%d` | %s |\n", file, line, text
            }
            text = ""; active = 0
          }
          {
            if (index($0, "[verify]") > 0 && index($0, "`[verify]`") == 0) {
              flush()
              t = substr($0, index($0, "[verify]") + 8)
              sub(/^[ \t:.-]+/, "", t); sub(/[ \t]*(\*\/|-->)?[ \t]*$/, "", t)
              # Code on the same line, or a short note: add context.
              before = substr($0, 1, index($0, "[verify]") - 1)
              sub("[ \t]*(" cm ")[ \t]*$", "", before); sub(/^[ \t]+/, "", before)
              if (before != "" && before !~ ("^(" cm ")")) {
                gsub(/`/, "", before)
                if (length(before) > 70) before = substr(before, 1, 67) "..."
                t = "`" before "`: " t
              } else if (before ~ ("^(" cm ")[ \t]*[^ \t]")) {
                sub("^(" cm ")[ \t]*", "", before)
                t = before " → " t
              } else if (length(t) < 50 && prev != "") {
                t = prev " → " t
              }
              text = t; line = NR; active = 1; next
            }
            p = $0
            if (p ~ ("^[ \t]*(" cm ")")) { sub("^[ \t]*(" cm ")[ \t]*", "", p); prev = p } else { prev = "" }
            if (active) {
              s = $0
              if (s ~ ("^[ \t]*(" cm ")[ \t]*[^ \t]")) {
                sub("^[ \t]*(" cm ")[ \t]*", "", s)
                text = text " " s
              } else {
                flush()
              }
            }
          }
          END { flush() }
        ' "$f"
      done
}

if [ "${1:-}" = --update ]; then
  doc=$2
  tmp=$(mktemp)
  table > "$tmp.table"
  awk -v tf="$tmp.table" '
    /<!-- BEGIN verify-index -->/ { print; while ((getline l < tf) > 0) print l; skip = 1; next }
    /<!-- END verify-index -->/ { skip = 0 }
    !skip { print }
  ' "$doc" > "$tmp"
  mv "$tmp" "$doc"
  rm -f "$tmp.table"
  echo "updated $doc ($(grep -c '^| `' "$doc") markers)"
else
  table
fi
