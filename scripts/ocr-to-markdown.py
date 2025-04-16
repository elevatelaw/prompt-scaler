# Usage:
#
#    uv run scripts/ocr-to-markdown.py INPUT_JSONL OUTPUT_DIR
#
# This will probably be replaced by one or more Rust modes once
# we nail down the details.

import os
import sys
import json
import re
from typing import List, Union


def main(input_jsonl: str, output_dir: str):
    if not os.path.exists(input_jsonl):
        print(f"Input file {input_jsonl} does not exist.")
        return

    if not os.path.exists(output_dir):
        os.makedirs(output_dir)

    with open(input_jsonl, "r") as f:
        for line in f:
            # Read a JSONL line.
            data = json.loads(line)
            id: str = data["id"]
            extracted_text: List[Union[str, None]] = data["pages"]

            # Insert markers for missing pages.
            final_text: List[str] = []
            for page in extracted_text:
                if page is None:
                    final_text.append("## (MISSING PAGE)")
                else:
                    final_text.append(page)
            final_text_combined: str = "\n\n".join(final_text)

            # Strip extension from ID, if any.
            filename = re.sub(r"\.[^.]+$", "", id)

            # Write to output_dir/filename.md.
            output_file: str = os.path.join(output_dir, f"{filename}.md")
            with open(output_file, "w") as out_f:
                out_f.write(final_text_combined)
                out_f.write("\n\n")

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: uv run scripts/ocr-to-markdown.py INPUT_JSONL OUTPUT_DIR")
        sys.exit(1)

    input_jsonl = sys.argv[1]
    output_dir = sys.argv[2]

    main(input_jsonl, output_dir)





