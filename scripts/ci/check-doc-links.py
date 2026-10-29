#!/usr/bin/env python3
"""Check repository-relative Markdown link destinations without network access.

External URLs and anchors are intentionally excluded. This catches renamed or
missing local guides and source references; it is not a rendered-anchor checker.
"""

from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit


def main():
    root = Path(__file__).resolve().parents[2]
    documents = sorted({*root.glob("README*.md"), *root.glob("docs/**/*.md")})
    failures = []
    checked = 0
    for document in documents:
        content = re.sub(r"```.*?```", "", document.read_text(), flags=re.DOTALL)
        for match in re.finditer(r"!?\[[^\]\n]*\]\(([^)\n]+)\)", content):
            target = match.group(1).strip()
            # Optional Markdown link titles do not form part of the path.
            if target.startswith("<"):
                target = target[1:target.index(">")]
            else:
                target = re.split(r'\s+[\"\']', target, maxsplit=1)[0]
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc or not parsed.path:
                continue
            checked += 1
            destination = (document.parent / unquote(parsed.path)).resolve()
            if not destination.is_relative_to(root) or not destination.exists():
                failures.append(f"{document.relative_to(root)}: {target}")
    if failures:
        print("Broken repository-relative Markdown links:", file=sys.stderr)
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"PASS: {checked} local link destinations in {len(documents)} Markdown files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
