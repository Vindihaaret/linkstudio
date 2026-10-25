#!/bin/sh
# Regenerates po/linkstudio.pot from the sources and merges it into every
# existing translation. To start a new language: cp linkstudio.pot <lang>.po
set -eu

cd "$(dirname "$0")/.."
# xgettext has no Rust parser; its C parser handles tr("…") calls fine, but
# warns about Rust lifetimes ('static), so those warnings are filtered out.
# Keep translatable strings on one line: Rust's "\" line continuation drops
# the next line's indentation, C's does not.
xgettext --language=C --from-code=UTF-8 --add-comments=Translators \
    --keyword=tr --keyword=trf --keyword=n_ \
    --package-name="Link Studio" --output=po/linkstudio.pot \
    $(find src -name '*.rs' | sort) 2>&1 | grep -v "unterminated" >&2 || true

for po in po/*.po; do
    [ -e "$po" ] || continue
    msgmerge --quiet --update --backup=none "$po" po/linkstudio.pot
    msgfmt --check --output-file=/dev/null "$po"
    echo "$po: $(msgfmt --statistics --output-file=/dev/null "$po" 2>&1)"
done
