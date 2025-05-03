# Compare the text files pointed by two columns of a CSV file.
#
# Usage:
#   uv run scripts/compare-text-path-columns.py [INPUT_CSV] [--out=OUTPUT_CSV] \
#       --column1=COLUMN1 --column2=COLUMN2
#
# The output will be written to a column named "jaccard_similarity" in the
# output CSV file.

import argparse
import csv
import os
import sys
from typing import TextIO

# Hack relative path imports to work, no matter what Guido prefers.
sys.path.append(os.path.abspath(os.path.join(os.path.dirname(__file__), "..")))

from support.doc_tokens import DocTokens

csv.field_size_limit(sys.maxsize)

def parse_arguments() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(description="Compare text files pointed by two columns of a CSV file.")
    parser.add_argument(
        "input_csv",
        type=argparse.FileType("r"),
        nargs="?",
        default=sys.stdin,
        help="Input CSV file",
    )
    parser.add_argument(
        "--out",
        type=argparse.FileType("w"),
        default=sys.stdout,
        help="Output CSV file (default: standard output)",
    )
    parser.add_argument(
        "--column1",
        type=str,
        required=True,
        help="Name of the first column in the CSV file to compare.",
    )
    parser.add_argument(
        "--column2",
        type=str,
        required=True,
        help="Name of the second column in the CSV file to compare.",
    )
    return parser.parse_args()

def main():
    args = parse_arguments()
    reader = csv.DictReader(args.input_csv)
    fieldnames = reader.fieldnames + ["jaccard_similarity"]
    writer = csv.DictWriter(args.out, fieldnames=fieldnames)
    writer.writeheader()
    column1: str = args.column1
    column2: str = args.column2

    for idx, row in enumerate(reader):
        if row[column1] and row[column2]:
            # Read the text files pointed by the two columns.
            with open(row[column1], "r") as file1:
                text1 = file1.read()
            with open(row[column2], "r") as file2:
                text2 = file2.read()

            # Calculate Jaccard similarity.
            tokens1 = DocTokens.from_text(text1)
            text1 = None
            tokens2 = DocTokens.from_text(text2)
            text2 = None
            row["jaccard_similarity"] = tokens1.jaqqard(tokens2)
        else:
            # If the columns are not present, set the similarity to None
            row["jaccard_similarity"] = None
        writer.writerow(row)

        # Every 1000 lines, print a progress message.
        count = idx + 1
        if count % 1000 == 0:
            print(f"Processed {count} lines.")
    print(f"Processed {count} lines.")

if __name__ == "__main__":
    main()
