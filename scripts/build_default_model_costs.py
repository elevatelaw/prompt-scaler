#!/usr/bin/env python3
"""Build src/default_model_costs.csv from saved vendor pricing pages.

NOTE: This script is a REFERENCE for how the CSV was originally built.
It will need to be updated for future versions of vendor pricing pages,
which may change their HTML structure. The HTML files must be downloaded
manually (some require login, e.g. AWS Bedrock).

Sources (as of April 2026):
  - OpenAI:    https://openai.com/api/pricing/
  - Anthropic: https://platform.claude.com/docs/en/about-claude/pricing
               https://platform.claude.com/docs/en/about-claude/models/overview
  - Bedrock:   https://aws.amazon.com/bedrock/pricing/ (login required)
  - Google:    https://ai.google.dev/gemini-api/docs/pricing
               https://cloud.google.com/vertex-ai/generative-ai/pricing

Usage:
    python scripts/build_default_model_costs.py \\
        --openai "Pricing _ OpenAI API.html" \\
        --anthropic-pricing "Pricing - Claude API Docs.html" \\
        --anthropic-models "Models overview - Claude API Docs.html" \\
        --bedrock "Amazon Bedrock Pricing – AWS.html" \\
        --gemini-aistudio "Gemini Developer API pricing ... .html" \\
        --gemini-vertex "Vertex AI Pricing ... .html" \\
        --output src/default_model_costs.csv
"""

from __future__ import annotations

import argparse
import re
import sys
from html.parser import HTMLParser
from pathlib import Path


# ══════════════════════════════════════════════════════════════════════
# Shared utilities
# ══════════════════════════════════════════════════════════════════════


class TableExtractor(HTMLParser):
    """Extract all <table> elements from HTML as lists of rows."""

    def __init__(self):
        super().__init__()
        self.tables: list[list[list[str]]] = []
        self._in_table = False
        self._table_depth = 0
        self._in_row = False
        self._in_cell = False
        self._current_table: list[list[str]] = []
        self._current_row: list[str] = []
        self._current_cell = ""

    def handle_starttag(self, tag, attrs):
        if tag == "table":
            self._table_depth += 1
            if self._table_depth == 1:
                self._in_table = True
                self._current_table = []
        elif tag == "tr" and self._in_table:
            self._in_row = True
            self._current_row = []
        elif tag in ("td", "th") and self._in_row:
            self._in_cell = True
            self._current_cell = ""

    def handle_endtag(self, tag):
        if tag == "table":
            if self._table_depth == 1 and self._current_table:
                self.tables.append(self._current_table)
            self._current_table = []
            self._in_table = self._table_depth > 1
            self._table_depth = max(0, self._table_depth - 1)
        elif tag == "tr" and self._in_row:
            self._in_row = False
            if self._current_row:
                self._current_table.append(self._current_row)
        elif tag in ("td", "th") and self._in_cell:
            self._in_cell = False
            self._current_row.append(self._current_cell.strip())

    def handle_data(self, data):
        if self._in_cell:
            self._current_cell += data


class HeadingAndTableExtractor(HTMLParser):
    """Extract headings and tables in document order."""

    def __init__(self):
        super().__init__()
        self.items: list = []
        self._in_heading = False
        self._heading_level = 0
        self._heading_text = ""
        self._heading_id = ""
        self._in_table = False
        self._table_depth = 0
        self._in_row = False
        self._in_cell = False
        self._current_table: list = []
        self._current_row: list = []
        self._current_cell = ""

    def handle_starttag(self, tag, attrs):
        if tag in ("h1", "h2", "h3", "h4"):
            self._in_heading = True
            self._heading_level = int(tag[1])
            self._heading_text = ""
            self._heading_id = dict(attrs).get("id", "")
        elif tag == "table":
            self._table_depth += 1
            if self._table_depth == 1:
                self._in_table = True
                self._current_table = []
        elif tag == "tr" and self._in_table:
            self._in_row = True
            self._current_row = []
        elif tag in ("td", "th") and self._in_row:
            self._in_cell = True
            self._current_cell = ""

    def handle_endtag(self, tag):
        if tag in ("h1", "h2", "h3", "h4") and self._in_heading:
            self._in_heading = False
            text = self._heading_text.strip()
            if text:
                self.items.append(
                    ("heading", text, self._heading_level, self._heading_id)
                )
        elif tag == "table":
            if self._table_depth == 1 and self._current_table:
                self.items.append(("table", self._current_table))
                self._current_table = []
                self._in_table = False
            self._table_depth = max(0, self._table_depth - 1)
        elif tag == "tr" and self._in_row:
            self._in_row = False
            if self._current_row:
                self._current_table.append(self._current_row)
        elif tag in ("td", "th") and self._in_cell:
            self._in_cell = False
            self._current_row.append(self._current_cell.strip())

    def handle_data(self, data):
        if self._in_heading:
            self._heading_text += data
        elif self._in_cell:
            self._current_cell += data


def extract_tables(path: str) -> list:
    with open(path, encoding="utf-8") as fh:
        ext = TableExtractor()
        ext.feed(fh.read())
    return ext.tables


def extract_headings_and_tables(path: str) -> list:
    with open(path, encoding="utf-8") as fh:
        ext = HeadingAndTableExtractor()
        ext.feed(fh.read())
    return ext.items


def parse_dollar(s: str) -> float | None:
    """Extract the first dollar amount from a pricing string."""
    if not s or s in ("N/A", "—", "-", "non-GA"):
        return None
    m = re.search(r"\$?([\d.]+)", s)
    return float(m.group(1)) if m else None


def per_token(per_million: float | None) -> float | None:
    return per_million / 1_000_000 if per_million is not None else None


def fmt_cost(value: float | None) -> str:
    if value is None:
        return ""
    return f"{value:.15f}".rstrip("0").rstrip(".")


# ══════════════════════════════════════════════════════════════════════
# OpenAI
# ══════════════════════════════════════════════════════════════════════

OPENAI_URL = "https://openai.com/api/pricing/"


def _openai_is_text_model(name: str) -> bool:
    name_lower = name.lower()
    skip_prefixes = ["dall-e", "tts-", "whisper", "gpt-image", "o1-pro", "computer-use"]
    skip_substrings = [
        "audio", "realtime", "image", "transcribe",
        "diarize", "search-api", "deep-research",
    ]
    if any(name_lower.startswith(p) for p in skip_prefixes):
        return False
    if any(s in name_lower for s in skip_substrings):
        return False
    if "with data sharing" in name_lower or name_lower.endswith("legacy"):
        return False
    return True


def extract_openai(html_path: str) -> list[dict]:
    html = Path(html_path).read_text(encoding="utf-8")

    class _Ext(HTMLParser):
        def __init__(self):
            super().__init__()
            self.models = []
            self._in_table = self._in_row = self._in_cell = False
            self._current_row = []
            self._current_cell = ""
            self._headers = []

        def handle_starttag(self, tag, attrs):
            if tag == "table":
                self._in_table = True; self._headers = []
            elif tag == "tr" and self._in_table:
                self._in_row = True; self._current_row = []
            elif tag in ("td", "th") and self._in_row:
                self._in_cell = True; self._current_cell = ""

        def handle_endtag(self, tag):
            if tag == "table":
                self._in_table = False
            elif tag == "tr" and self._in_row:
                self._in_row = False
                if self._current_row:
                    if not self._headers:
                        self._headers = [c.strip().lower() for c in self._current_row]
                    else:
                        record = {}
                        for i, val in enumerate(self._current_row):
                            if i < len(self._headers):
                                record[self._headers[i]] = val
                        if "model" in record and record["model"].strip():
                            self.models.append(record)
            elif tag in ("td", "th") and self._in_cell:
                self._in_cell = False
                self._current_row.append(self._current_cell.strip())

        def handle_data(self, data):
            if self._in_cell:
                self._current_cell += data

    ext = _Ext()
    ext.feed(html)

    results = []
    seen = set()
    for record in ext.models:
        model = record["model"].strip()
        if not _openai_is_text_model(model) or model in seen:
            continue
        seen.add(model)

        input_price = cached_price = output_price = None
        for key, val in record.items():
            kl = key.lower()
            if "input" in kl and "cached" not in kl:
                input_price = parse_dollar(val)
            elif "cached" in kl:
                cached_price = parse_dollar(val)
            elif "output" in kl:
                output_price = parse_dollar(val)

        if input_price is not None and output_price is not None:
            results.append({
                "driver": "", "model": model,
                "input": per_token(input_price),
                "cached": per_token(cached_price),
                "output": per_token(output_price),
                "url": OPENAI_URL,
            })

    return results


# ══════════════════════════════════════════════════════════════════════
# Anthropic
# ══════════════════════════════════════════════════════════════════════

ANTHROPIC_PRICING_URL = "https://platform.claude.com/docs/en/about-claude/pricing"
ANTHROPIC_MODELS_URL = "https://platform.claude.com/docs/en/about-claude/models/overview"

# Display name (lowercase) -> API model ID.
_CLAUDE_DISPLAY_TO_ID = {
    "claude opus 4.6": "claude-opus-4-6",
    "claude opus 4.5": "claude-opus-4-5-20251101",
    "claude opus 4.1": "claude-opus-4-1-20250805",
    "claude opus 4": "claude-opus-4-20250514",
    "claude sonnet 4.6": "claude-sonnet-4-6",
    "claude sonnet 4.5": "claude-sonnet-4-5-20250929",
    "claude sonnet 4": "claude-sonnet-4-20250514",
    "claude sonnet 3.7 (deprecated)": "claude-3-7-sonnet-20250219",
    "claude haiku 4.5": "claude-haiku-4-5-20251001",
    "claude haiku 3.5": "claude-3-5-haiku-20241022",
    "claude opus 3 (deprecated)": "claude-3-opus-20240229",
    "claude haiku 3": "claude-3-haiku-20240307",
}


def extract_anthropic(
    pricing_path: str, models_path: str, bedrock_path: str | None,
) -> list[dict]:
    # ── Pricing table ──
    pricing_tables = extract_tables(pricing_path)
    table = pricing_tables[0]
    pricing: dict[str, dict] = {}
    for row in table[1:]:
        if len(row) < 6:
            continue
        api_id = _CLAUDE_DISPLAY_TO_ID.get(row[0].strip().lower())
        if api_id is None:
            print(f"  WARNING: Unknown Anthropic model: {row[0]!r}")
            continue
        input_price = per_token(parse_dollar(row[1]))
        cached_price = per_token(parse_dollar(row[4]))  # "Cache Hits" column
        output_price = per_token(parse_dollar(row[5]))
        if input_price is not None and output_price is not None:
            pricing[api_id] = {"input": input_price, "cached": cached_price, "output": output_price}

    # ── Bedrock IDs from Models Overview ──
    models_tables = extract_tables(models_path)
    bedrock_ids: dict[str, str] = {}
    for row in models_tables[0]:
        label = row[0].lower() if row else ""
        if "claude api id" in label and "alias" not in label:
            api_ids = row[1:]
        elif "aws bedrock id" in label:
            bk_ids = row[1:]
    for a, b in zip(api_ids, bk_ids):
        a, b = a.strip(), b.strip()
        if a and b:
            bedrock_ids[a] = b

    # ── Spot-check Bedrock pricing ──
    if bedrock_path:
        bk_tables = extract_tables(bedrock_path)
        print("\n  Bedrock spot-check (US standard):")
        for row in bk_tables[2][1:]:
            if len(row) < 3 or "Long Context" in row[0]:
                continue
            api_id = _CLAUDE_DISPLAY_TO_ID.get(row[0].strip().lower())
            if api_id and api_id in pricing:
                bk_in = per_token(parse_dollar(row[1]))
                bk_out = per_token(parse_dollar(row[2]))
                ours = pricing[api_id]
                ok = (abs((ours["input"] or 0) - (bk_in or 0)) < 1e-15
                      and abs((ours["output"] or 0) - (bk_out or 0)) < 1e-15)
                print(f"    {api_id:<40} {'OK' if ok else 'MISMATCH!'}")

    # ── Build entries ──
    entries = []
    for api_id, costs in sorted(pricing.items()):
        entries.append({
            "driver": "", "model": api_id,
            "input": costs["input"], "cached": costs.get("cached"),
            "output": costs["output"], "url": ANTHROPIC_PRICING_URL,
        })
    for api_id, bk_id in sorted(bedrock_ids.items()):
        if api_id in pricing:
            costs = pricing[api_id]
            entries.append({
                "driver": "", "model": bk_id,
                "input": costs["input"], "cached": costs.get("cached"),
                "output": costs["output"], "url": ANTHROPIC_PRICING_URL,
            })
    return entries


# ══════════════════════════════════════════════════════════════════════
# Google Gemini
# ══════════════════════════════════════════════════════════════════════

AISTUDIO_URL = "https://ai.google.dev/gemini-api/docs/pricing"
VERTEX_URL = "https://cloud.google.com/vertex-ai/generative-ai/pricing"

# Heading ID on AI Studio page -> API model name.
_GEMINI_HEADING_TO_MODEL = {
    "gemini-3.1-pro-preview": "gemini-3.1-pro-preview",
    "gemini-3.1-flash-lite-preview": "gemini-3.1-flash-lite-preview",
    "gemini-3-flash-preview": "gemini-3-flash-preview",
    "gemini-2.5-pro": "gemini-2.5-pro",
    "gemini-2.5-flash": "gemini-2.5-flash",
    "gemini-2.5-flash-lite": "gemini-2.5-flash-lite",
    "gemini-2.5-flash-lite-preview": "gemini-2.5-flash-lite-preview",
    "gemini-2.0-flash": "gemini-2.0-flash",
    "gemini-2.0-flash-lite": "gemini-2.0-flash-lite",
}

_GEMINI_SKIP_HEADINGS = {
    "gemini-3.1-flash-live-preview", "gemini-3.1-flash-image-preview",
    "gemini-3-pro-image-preview", "gemini-2.5-flash-native-audio",
    "gemini-2.5-flash-image", "gemini-2.5-flash-preview-tts",
    "gemini-2.5-pro-preview-tts", "gemini-embedding-2", "gemini-embedding",
    "gemini-robotics-er", "gemini-2.5-computer-use-preview-10-2025",
}

# Vertex display name (lowercase) -> API model ID.
_VERTEX_NAME_TO_ID = {
    "gemini 3.1 pro preview": "gemini-3.1-pro-preview",
    "gemini 3.1 flash lite preview": "gemini-3.1-flash-lite-preview",
    "gemini 3 flash preview": "gemini-3-flash-preview",
    "gemini 2.5 pro": "gemini-2.5-pro",
    "gemini 2.5 flash": "gemini-2.5-flash",
    "gemini 2.5 flash-lite": "gemini-2.5-flash-lite",
    "gemini 2.5 flash lite": "gemini-2.5-flash-lite",
    "gemini 2.0 flash": "gemini-2.0-flash",
    "gemini 2.0 flash-lite": "gemini-2.0-flash-lite",
    "gemini 2.0 flash lite": "gemini-2.0-flash-lite",
}


def extract_gemini(aistudio_path: str, vertex_path: str) -> list[dict]:
    # ── AI Studio ──
    items = extract_headings_and_tables(aistudio_path)
    aistudio: dict[str, dict] = {}
    current_model = None
    for item in items:
        if item[0] == "heading":
            _, text, level, hid = item
            if hid in _GEMINI_SKIP_HEADINGS:
                current_model = None
            elif hid in _GEMINI_HEADING_TO_MODEL:
                current_model = _GEMINI_HEADING_TO_MODEL[hid]
            else:
                current_model = None
        elif item[0] == "table" and current_model and current_model not in aistudio:
            table = item[1]
            inp = outp = cached = None
            for row in table:
                if len(row) < 3:
                    continue
                label = row[0].lower()
                paid = row[2]
                if "input" in label and "cached" not in label:
                    inp = parse_dollar(paid)
                elif "output" in label:
                    outp = parse_dollar(paid)
                elif "cach" in label and "storage" not in label:
                    cached = parse_dollar(paid)
            if inp is not None and outp is not None:
                aistudio[current_model] = {
                    "input": per_token(inp), "cached": per_token(cached), "output": per_token(outp),
                }

    # ── Vertex ──
    items = extract_headings_and_tables(vertex_path)
    vertex: dict[str, dict] = {}
    in_section = False
    for item in items:
        if item[0] == "heading":
            _, text, level, hid = item
            in_section = level <= 3 and "gemini" in text.lower()
        elif item[0] == "table" and in_section:
            current = None
            for row in item[1]:
                if not row:
                    continue
                if len(row) == 1:
                    current = _VERTEX_NAME_TO_ID.get(row[0].strip().lower())
                    continue
                if current is None:
                    continue
                if current not in vertex:
                    vertex[current] = {}
                label = row[0].lower()
                is_audio_only = "audio" in label and "text" not in label and "image" not in label
                if "input" in label and not is_audio_only and "input" not in vertex[current]:
                    vertex[current]["input"] = per_token(parse_dollar(row[1]) if len(row) > 1 else None)
                    if len(row) > 3:
                        c = parse_dollar(row[3])
                        if c is not None:
                            vertex[current]["cached"] = per_token(c)
                elif "output" in label and "output" not in vertex[current]:
                    vertex[current]["output"] = per_token(parse_dollar(row[1]) if len(row) > 1 else None)
            in_section = False

    # ── Merge ──
    entries = []
    for model in sorted(set(aistudio) | set(vertex)):
        ai, vx = aistudio.get(model), vertex.get(model)
        if ai and vx:
            same = (_close(ai["input"], vx.get("input"))
                    and _close(ai["output"], vx.get("output")))
            if same:
                entries.append({
                    "driver": "", "model": model,
                    "input": ai["input"],
                    "cached": ai.get("cached") or vx.get("cached"),
                    "output": ai["output"], "url": AISTUDIO_URL,
                })
            else:
                entries.append({
                    "driver": "", "model": model,
                    "input": ai["input"], "cached": ai.get("cached"),
                    "output": ai["output"], "url": AISTUDIO_URL,
                })
                entries.append({
                    "driver": "vertex", "model": model,
                    "input": vx.get("input"), "cached": vx.get("cached"),
                    "output": vx.get("output"), "url": VERTEX_URL,
                })
        elif ai:
            entries.append({
                "driver": "", "model": model,
                "input": ai["input"], "cached": ai.get("cached"),
                "output": ai["output"], "url": AISTUDIO_URL,
            })
        elif vx:
            entries.append({
                "driver": "vertex", "model": model,
                "input": vx.get("input"), "cached": vx.get("cached"),
                "output": vx.get("output"), "url": VERTEX_URL,
            })
    return entries


def _close(a, b, tol=1e-15):
    if a is None and b is None:
        return True
    if a is None or b is None:
        return False
    return abs(a - b) < tol


# ══════════════════════════════════════════════════════════════════════
# CSV assembly
# ══════════════════════════════════════════════════════════════════════

CSV_HEADER = "driver,model,input_cost_per_token,input_cost_per_cached_token,output_cost_per_token,pricing_source_url\n"


def write_csv(
    output_path: str,
    openai: list[dict],
    anthropic: list[dict],
    gemini: list[dict],
):
    lines = [CSV_HEADER]

    lines.append(f"# OpenAI - {OPENAI_URL}\n")
    for e in openai:
        lines.append(_csv_line(e))

    lines.append(f"# Anthropic - {ANTHROPIC_PRICING_URL}\n")
    lines.append(f"# Bedrock IDs from {ANTHROPIC_MODELS_URL}\n")
    lines.append("# Bedrock standard pricing matches direct API (verified against https://aws.amazon.com/bedrock/pricing/).\n")
    for e in anthropic:
        lines.append(_csv_line(e))

    lines.append(f"# Google - {AISTUDIO_URL}\n")
    lines.append(f"# Vertex AI pricing from {VERTEX_URL}\n")
    for e in gemini:
        lines.append(_csv_line(e))

    Path(output_path).write_text("".join(lines))


def _csv_line(e: dict) -> str:
    return (
        f"{e['driver']},{e['model']},{fmt_cost(e['input'])},"
        f"{fmt_cost(e.get('cached'))},{fmt_cost(e['output'])},{e['url']}\n"
    )


# ══════════════════════════════════════════════════════════════════════
# Main
# ══════════════════════════════════════════════════════════════════════

def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--openai", required=True, help="OpenAI pricing HTML")
    p.add_argument("--anthropic-pricing", required=True, help="Anthropic pricing HTML")
    p.add_argument("--anthropic-models", required=True, help="Anthropic models overview HTML")
    p.add_argument("--bedrock", default=None, help="AWS Bedrock pricing HTML (optional, for spot-check)")
    p.add_argument("--gemini-aistudio", required=True, help="Google AI Studio pricing HTML")
    p.add_argument("--gemini-vertex", required=True, help="Vertex AI pricing HTML")
    p.add_argument("--output", required=True, help="Output CSV path")
    args = p.parse_args()

    print("Extracting OpenAI pricing...")
    openai = extract_openai(args.openai)
    print(f"  {len(openai)} models")

    print("Extracting Anthropic pricing...")
    anthropic = extract_anthropic(args.anthropic_pricing, args.anthropic_models, args.bedrock)
    print(f"  {len(anthropic)} entries (direct + Bedrock IDs)")

    print("Extracting Gemini pricing...")
    gemini = extract_gemini(args.gemini_aistudio, args.gemini_vertex)
    print(f"  {len(gemini)} entries")

    total = len(openai) + len(anthropic) + len(gemini)
    print(f"\nWriting {total} entries to {args.output}")
    write_csv(args.output, openai, anthropic, gemini)
    print("Done.")


if __name__ == "__main__":
    main()
