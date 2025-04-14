# Usage:
#
#    uv run scripts/compare-models.py DATA_DIR
#
# For all image and PDF files in DATA_DIR, perform OCR with all
# major models and output the results as Markdown files.

import csv
from dataclasses import dataclass
import os
import subprocess
import sys
import json
import re
from typing import List, Mapping, Set, Union

@dataclass
class ModelInfo:
    """Information about each OCR mdoel to test."""

    name: str
    """The name of the model."""

    rasterize: bool
    """Do we need to rasterize input PDFs?"""

    job_count: int = 30
    """How many jobs to run in parallel."""

    def options(self) -> List[str]:
        """Return the command line options for this model."""
        opts = ["--model", self.name, "--jobs", str(self.job_count)]
        if self.rasterize:
            opts.append("--rasterize")
        return opts
    
    def build_path(self, filename: str, ext: str) -> str:
        """Build a path to an output file for this model."""
        return f"{filename}.{self.name}.{ext}"

CPU_COUNT: int = os.cpu_count() or 8
"""The number of CPU cores available."""

MODELS: List[ModelInfo] = [
    ModelInfo("gemini-2.0-flash", False),
    ModelInfo("textract", False, 8), # Max 10/second.
    ModelInfo("tesseract", True, CPU_COUNT),
    ModelInfo("pdftotext", False, CPU_COUNT),
]

INPUT_EXTENSIONS: Set[str] = {"pdf", "jpg", "jpeg", "png", "webp", "gif"}
"""Known input extensions."""

OUTPUT_EXTENSIONS: Set[str] = {"csv", "jsonl", "md", "html"}
"""Known output extensions."""


def main(data_dir: str) -> None:
    if not os.path.isdir(data_dir):
        raise Exception(f"Data directory {data_dir} does not exist.")

    print(f"Detected {CPU_COUNT} CPU cores.", file=sys.stderr)

    # Find each input file in the data_dir.
    input_csv_path: str = os.path.join(data_dir, "input.csv")
    with open(input_csv_path, "w") as f:
        csv_writer = csv.writer(f)
        # Write the header.
        csv_writer.writerow(["id", "path"])

        for root, _, files in os.walk(data_dir):
            for rel_path in files:
                ext = rel_path.rsplit(".", 1)[-1].lower()

                # Check if we should skip this file.
                if ext in OUTPUT_EXTENSIONS:
                    continue
                if ext not in INPUT_EXTENSIONS:
                    print(f"Skipping {rel_path} (unknown extension)", file=sys.stderr)
                    continue
                
                # Add the file to the input list.
                csv_writer.writerow([rel_path, rel_path])

    # Build our token count matrix. This is keyed by model name,
    # then by relative path to the input file, and then by token.
    model_file_token_counts: Mapping[str, Mapping[str, Mapping[str, int]]] = {}

    # Run each OCR engine and report per-file results.
    for model in MODELS:
        print(f"Running {model.name}...", file=sys.stderr)
        output_jsonl_path = os.path.join(data_dir, model.build_path("output", "jsonl"))

        # Run the OCR engine, passing command-line arguments as an array.
        #
        # We allow a very high failure rate in case some of the OCR engines
        # are unable to handle certain input formats.
        cmd = ["prompt-scaler", "ocr", "--allowed-failure-rate", "1.0"]
        cmd.extend(model.options())
        cmd.extend(["--out", os.path.basename(output_jsonl_path), os.path.basename(input_csv_path)])
        subprocess.run(cmd, cwd=data_dir, check=True)

        # Output the results of each model.
        file_token_counts = output_results(data_dir, model, output_jsonl_path)
        model_file_token_counts[model.name] = file_token_counts

    # Swap the model and file names in the token count matrix.
    file_model_token_counts: Mapping[str, Mapping[str, Mapping[str, int]]] = {}
    for model_name, file_token_counts in model_file_token_counts.items():
        for rel_path, token_counts in file_token_counts.items():
            if rel_path not in file_model_token_counts:
                file_model_token_counts[rel_path] = {}
            file_model_token_counts[rel_path][model_name] = token_counts

    # Write the final token count matrix for each file.
    for rel_path, model_token_counts in file_model_token_counts.items():
        matrix_html_path = os.path.join(data_dir, rel_path + ".html")
        output_matrix(matrix_html_path, model_token_counts)

def output_results(data_dir: str, model: ModelInfo, output_jsonl_path: str) -> Mapping[str, Mapping[str, int]]:
    """Output the results for each OCRed file. This should include:
    
    1. A single Markdown file for each input file, with the extracted text.
    2. A CSV file containing sorted `token,count` pairs.
    """

    file_token_counts: Mapping[str, Mapping[str, int]] = {}
    with open(output_jsonl_path, "r", encoding="utf-8") as f:
        for line in f:
            # Read a JSONL line.
            data = json.loads(line)
            rel_path: str = data["id"]
            extracted_text: List[Union[str, None]] = data["extracted_text"]

            # Write our Markdown output.
            markdown_output_path: str = os.path.join(data_dir, model.build_path(rel_path, "md"))
            output_markdown(markdown_output_path, extracted_text)

            # Write our token output.
            token_output_path: str = os.path.join(data_dir, model.build_path(rel_path, "tokens.csv"))
            matrix = output_tokens(token_output_path, extracted_text)
            file_token_counts[rel_path] = matrix
    return file_token_counts

def output_markdown(output_path: str, extracted_text: List[Union[str, None]]) -> None:
    """Output the extracted text as a Markdown file."""
    
    # Insert markers for missing pages.
    final_text: List[str] = []
    for page in extracted_text:
        if page is None:
            final_text.append("## (MISSING PAGE)")
        else:
            final_text.append(page)
    final_text_combined: str = "\n\n".join(final_text)

    with open(output_path, "w") as out_f:
        out_f.write(final_text_combined)

def output_tokens(output_path: str, extracted_text: List[Union[str, None]]) -> Mapping[str, int]:
    """Output the extracted text as a CSV file containing `token,count` pairs,
    sorted by token in UTF-8 lexical order."""

    final_text = "\n".join(t for t in extracted_text if t is not None)
    tokens = re.findall(r"\b\w+\b", final_text)

    # Count the tokens.
    token_counts = {}
    for token in tokens:
        token_counts[token] = token_counts.get(token, 0) + 1
    sorted_tokens = sorted(token_counts.items(), key=lambda x: x[0])

    # Write the CSV file.
    with open(output_path, "w", newline="") as out_f:
        csv_writer = csv.writer(out_f)
        csv_writer.writerow(["token", "count"])
        for token, count in sorted_tokens:
            csv_writer.writerow([token, count])

    return token_counts

def output_matrix(output_path: str, model_token_counts: Mapping[str, Mapping[str, int]]) -> None:
    """Output the token count matrix as an HTML file."""

    model_names = [model.name for model in MODELS]
    total_token_count = 0
    some_model_name = next(iter(model_token_counts.keys()))
    for _token, count in model_token_counts[some_model_name].items():
        total_token_count += count

    with open(output_path, "w") as out_f:
        out_f.write("<html>\n")
        out_f.write(f"<head><title>{output_path}</title></head>\n")
        out_f.write("<body>\n")
        out_f.write(f"<h1>{output_path}</h1>\n")
        out_f.write("<h2>Token Diff Matrix</h2>\n")

        out_f.write(f"<p>Total token count: {total_token_count}</p>\n")

        out_f.write("<table border=\"1\">\n")

        out_f.write(f"<tr><th>&nbsp;</th><th colspan=\"{len(model_names)}\">Diff With</th></tr>\n")

        out_f.write("<tr><th>Model</th>")
        for model_name in model_names:
            out_f.write(f"<th><tt>{model_name}</tt></th>")
        out_f.write("</tr>\n")

        for model_name in model_names:
            out_f.write(f"<tr><th><tt>{model_name}</tt></th>")
            for other_model_name in model_names:
                if model_name == other_model_name:
                    out_f.write("<td>&nbsp;</td>")
                    continue
            
                # Recompute the token counts for this model.
                total_token_count = sum(
                    model_token_counts[model_name].values()
                )

                # Compare the token counts and output the diff.
                token_counts_a = model_token_counts[model_name]
                token_counts_b = model_token_counts[other_model_name]
                diff = compare_token_counts(token_counts_a, token_counts_b)
                added_percent = round(safe_div(100.0 * diff.added, total_token_count), 2)
                removed_percent = round(safe_div(100.0 * diff.removed, total_token_count), 2)

                out_f.write(f"<td><span style='color:green;'>+{diff.added} (+{added_percent}%)</span> / <span style='color:red;'>-{diff.removed} (-{removed_percent}%) </span></td>")
            out_f.write("</tr>\n")
        out_f.write("</table>\n")
        out_f.write("</body>\n")
        out_f.write("</html>\n")

@dataclass
class TokenCountDiff:
    """A diff between two token counts."""

    added: int
    """How many tokens were added."""
    removed: int
    """How many tokens were removed."""

def compare_token_counts(
    token_counts_a: Mapping[str, int],
    token_counts_b: Mapping[str, int]
) -> TokenCountDiff:
    """Compare two token counts and return the diff."""

    # Count the number of tokens that were added or removed.
    added = 0
    removed = 0
    for token, count in token_counts_a.items():
        if token in token_counts_b:
            added += max(0, token_counts_b[token] - count)
            removed += max(0, count - token_counts_b[token])
        else:
            removed += count

    for token, count in token_counts_b.items():
        if token not in token_counts_a:
            added += count

    return TokenCountDiff(added=added, removed=removed)

def safe_div(a: float, b: float) -> float:
    """Return a / b, but return NaN if b is 0."""
    if b == 0:
        return float("nan")
    else:
        return a / b

if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("Usage: uv run scripts/compare-models.py DATA_DIR")
        sys.exit(1)

    data_dir = sys.argv[1]
    main(data_dir)





