# Usage:
#
#    uv run scripts/ocr-benchmark.py [--images=N] [--model=MODEL] [--jobs=N]
#
#

from __future__ import annotations

import argparse
import csv
from enum import Enum
import json
import os
from pathlib import Path
import re
import sys
from typing import Any, List, Optional

import bleach
from jinja2 import Environment, FileSystemLoader, select_autoescape

import markdown
from markupsafe import Markup
from pydantic import BaseModel, ConfigDict, Field, field_validator
from pydantic.alias_generators import to_camel

# Hack relative path imports to work, no matter what Guido prefers.
sys.path.append(os.path.abspath(os.path.join(os.path.dirname(__file__), "..")))

from support.ocr_benchmark import OCR_BENCHMARK_PATH, OCR_BENCHMARK_TEST_PATH, BenchmarkImage
from support.jinja_filters import json_filter, markdown_filter
from support.jsonl import jsonl_records
from support.models.ocr_output import OcrOutput

def parse_arguments() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(description="OCR Benchmarking Script")
    parser.add_argument(
        "--images",
        type=int,
        default=100,
        help="Number of images to process (default: 100)",
    )
    parser.add_argument(
        "--model",
        type=str,
        default="gemini-2.0-flash",
        help="OCR model to use (default: gemini-2.0-flash)",
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=50,
        help="Number of parallel jobs to run (default: 50)",
    )
    return parser.parse_args()

def main():
    """Main function to run the OCR benchmark."""
    args = parse_arguments()

    # If our benchmark directory doesn't exist, tell the user they
    # need to check it out.
    if not OCR_BENCHMARK_TEST_PATH.exists():
        print(
            f"Benchmark directory {OCR_BENCHMARK_TEST_PATH} does not exist. "
            "Please check out the benchmark repository using "
            "`git submodule update --init` (requires `git lfs`)",
            file=sys.stderr,
        )
        return

    # Load the benchmark images.
    images = BenchmarkImage.load_all()
    print(f"Loaded {len(images)} images.")

    # Limit the number of images to process.
    if args.images > len(images):
        args.images = len(images)
    print(f"Will process {args.images} images.")
    images = images[:args.images]

    # Load our OCR output for various models, which is stored as
    # OCR_BENCHMARK_PATH / "output.$MODEL.jsonl". We glob for it.
    model_names = []
    for model in OCR_BENCHMARK_PATH.glob("output-*.jsonl"):
        model_name = re.sub(r"^output-(.*)\.jsonl$", r"\1", model.name)
        model_names.append(model_name)
        outputs = list(jsonl_records(OcrOutput, model))
        if len(outputs) < len(images):
            print(
                f"Warning: {model_name} output has fewer images ({len(outputs)}) "
                f"than benchmark images ({len(images)}).",
                file=sys.stderr,
            )
        for output in outputs:
            images[int(output.id)].add_model_results(model_name, output)
        print(f"Loaded {len(outputs)} outputs for model {model_name}.")

    # Load our notes about "bad ground truth data".
    with open(OCR_BENCHMARK_PATH / "bad_ground_truth.csv", "r") as f:
        rdr = csv.reader(f)
        # Skip the header.
        next(rdr)
        for row in rdr:
            if len(row) != 2:
                print(f"Invalid row in bad ground truth data: {row}", file=sys.stderr)
                continue
            image_id = int(row[0])
            ground_truth_issue = row[1]
            if image_id >= len(images):
                print(
                    f"Invalid image ID {image_id} in bad ground truth data: {row}",
                    file=sys.stderr,
                )
                continue
            images[image_id].ground_truth_issue = ground_truth_issue

    # Collect Jaqqard scores for each model.
    jaqqard_scores = {}
    for model_name in model_names:
        jaqqard_scores[model_name] = []
        for image in images:
            if model_name in image.model_results:
                model_result = image.model_results[model_name]
                if image.ground_truth_issue is not None:
                    continue
                jaqqard_scores[model_name].append(model_result.jaqqard_similarity)
    avg_jaqqard_scores = {}
    for model_name in model_names:
        avg_jaqqard_scores[model_name] = sum(jaqqard_scores[model_name]) / len(
            jaqqard_scores[model_name]
        )
        print(f"Average Jaqqard score for {model_name}: {avg_jaqqard_scores[model_name]}")

    # Sort model names by average Jaqqard score, descending.
    model_names = sorted(
        model_names,
        key=lambda x: avg_jaqqard_scores[x],
        reverse=True,
    )

    # Write our images to CSV file with id, path.
    with open(OCR_BENCHMARK_PATH / "input.csv", "w") as f:
        wtr = csv.writer(f)
        wtr.writerow(["id", "path"])
        for image in images[:args.images]:
            wtr.writerow([image.id, image.path()])

    # Write an HTML file summarizing the benchmark.
    env = Environment(
        loader=FileSystemLoader(OCR_BENCHMARK_PATH / "templates"),
        autoescape=select_autoescape(
            default_for_string=True,
            default=True,
        ),
    )
    env.filters["json"] = json_filter
    env.filters["md"] = markdown_filter
    template = env.get_template("results.html.j2")
    with open(OCR_BENCHMARK_PATH / "results.html", "w") as f:
        f.write(template.render(
            images=images[:args.images],
            models=model_names,
            avg_jaqqard_scores=avg_jaqqard_scores,
        ))

if __name__ == "__main__":
    main()
