# Export a key of a JSONL file to individual files on disk.
#
# Usage:
#   uv run scripts/jsonl-key-to-text-files.py [INPUT_JSONL] [--out=OUTPUT_JSONL]
#       --key=KEY --new-key=NEW_KEY --path-pattern=PATTERN
#
# Options:
#   INPUT_JSONL             Input JSONL file or standard input.
#   --out=OUTPUT_JSONL      Output JSONL file or standard output.
#   --key=KEY               Name of the key in the JSONL file to export.
#   --new-key=NEW_KEY       Name of the new key to create in the JSONL file, containing the file paths.
#   --path-pattern=PATTERN  Pattern for output file names. Use "{id}" to insert the ID of the row.
#
# If `key` is not found in the JSON object, the line will be written to the
# output file as is.

import argparse
import json
import os
import sys
from typing import TextIO

def parse_arguments() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(description="Export a column of a CSV file to individual files.")
    parser.add_argument(
        "input_jsonl",
        type=argparse.FileType("r"),
        nargs="?",
        default=sys.stdin,
        help="Input JSONL file",
    )
    parser.add_argument(
        "--out",
        type=argparse.FileType("w"),
        default=sys.stdout,
        help="Output JSONL file (default: standard output)",
    )
    parser.add_argument(
        "--key",
        type=str,
        required=True,
        help="Name of the key in the JSONL file to export.",
    )
    parser.add_argument(
        "--new-key",
        type=str,
        required=True,
        help="Name of the new key to create in the JSONL file, containing the file paths.",
    )
    parser.add_argument(
        "--path-pattern",
        type=str,
        required=True,
        help="Pattern for output file names. Use '{id}' to insert the ID of the row.",
    )
    return parser.parse_args()

def main():
    """Main function to export a column of a JSONL file to individual files."""
    args = parse_arguments()
    input_jsonl: TextIO = args.input_jsonl
    output_jsonl: TextIO = args.out
    key: str = args.key
    new_key: str = args.new_key
    path_pattern: str = args.path_pattern

    # Read the JSONL file line by line.
    for idx, line in enumerate(input_jsonl):
        # Parse the JSON line.
        data = json.loads(line)

        # Get the value of the specified key. If it doesn't exist, write the line as is.
        value = data.get(key)
        if value is None:
            output_jsonl.write(line)
            continue

        # Create the output file path using the pattern and the ID of the row.
        output_path = path_pattern.format(id=data["id"])
        # Make sure the output directory exists.
        output_dir = output_path.rsplit("/", 1)[0]
        if output_dir:
            os.makedirs(output_dir, exist_ok=True)
        # Write the value to the output file.
        with open(output_path, "w") as output_file:
            output_file.write(value)
        
        # Add the new key to the JSON object with the file path,
        # and remove the original key.
        data[new_key] = output_path
        data.pop(key, None)

        # Write the modified JSON object to the output file.
        output_jsonl.write(json.dumps(data) + "\n")

        # Every 1000 lines, print a progress message.
        count = idx + 1
        if count % 1000 == 0:
            print(f"Processed {count} lines.")
    print(f"Processed {count} lines.")

if __name__ == "__main__":
    main()
