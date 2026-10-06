"""Check product Markdown links and reject accidental machine-specific references."""
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
files = [ROOT / "README.md", ROOT / "THIRD_PARTY_NOTICES.md", *sorted((ROOT / "docs").glob("*.md"))]
errors = []
for file in files:
    text = file.read_text(encoding="utf-8")
    for target in re.findall(r"\]\(([^\s)]+)(?:\s+[^)]*)?\)", text):
        if target.startswith(("https://", "http://", "mailto:", "#")):
            continue
        path = (file.parent / target.split("#", 1)[0]).resolve()
        if not path.is_relative_to(ROOT) or not path.is_file():
            errors.append(f"{file.relative_to(ROOT)}: missing local link {target}")
    for pattern in (r"C:[\\/]Users[\\/]", r"github\.com/[A-Za-z0-9-]+_microsoft/", r"current source: 0\.1\.1"):
        if re.search(pattern, text, re.IGNORECASE):
            errors.append(f"{file.relative_to(ROOT)}: private or stale reference {pattern}")
if errors:
    raise SystemExit("\n".join(errors))
print(f"Product documentation verified: {len(files)} Markdown files.")
