"""Support for the benchmarks/ocr benchmark."""

from __future__ import annotations

import json
from enum import Enum
from pathlib import Path
from typing import Any, Dict, List, Optional, Set

from pydantic import BaseModel, ConfigDict, Field, field_validator
from pydantic.alias_generators import to_camel

from .doc_tokens import DocTokens
from .jsonl import jsonl_records
from .models.ocr_output import OcrOutput

BENCHMARK_PATH = Path("benchmarks")
"""Path to our benchmarks."""

OCR_BENCHMARK_PATH = BENCHMARK_PATH / "ocr"
"""Path to our OCR benchmark."""

OCR_BENCHMARK_TEST_PATH = OCR_BENCHMARK_PATH / "data/test/"
"""Metadata for benchmark images."""

class DocumentQuality(str, Enum):
    """Enum for document quality.
    
    This is a strange breakdown, but it's what the benchmark uses."""
    
    CLEAN = "CLEAN"
    PHOTO = "PHOTO"
    HIGH_QUALITY = "HIGH_QUALITY"
    LOW_QUALITY = "LOW_QUALITY"

class BenchmarkMetadata(BaseModel):
    """Pydantic model for loading and using image metadata."""
    # Don't allow unknown fields.
    model_config = ConfigDict(extra="forbid", alias_generator=to_camel)

    format: str
    """Format of the image (e.g., 'CHART', 'PATENT', etc.)."""

    document_quality: DocumentQuality
    """Quality of the document (e.g., 'CLEAN', 'PHOTO', etc.)."""

    font_family: Optional[str] = Field(default=None)
    """Font family used in the image (if applicable)."""

    rotation: Optional[int] = Field(default=None)
    """Rotation of the image in degrees (if applicable)."""

class BenchmarkImage(BaseModel):
    """Pydantic model for loading and using benchmark metadata."""

    # Don't allow unknown fields.
    model_config = ConfigDict(extra="forbid")

    id: int
    """Image ID"""

    metadata: BenchmarkMetadata
    """Metadata for the image."""

    json_schema: str
    """JSON schema for extracting information from the image."""

    true_json_output: str
    """Correct JSON data to extract from the image."""

    true_markdown_output: str
    """Correct Markdown data to extract from the image."""

    file_name: str
    """File name of the image."""

    ground_truth_issue: Optional[str] = Field(default=None)
    """Ground truth issue for the image (if applicable)."""

    model_results: Dict[str, ModelResults] = {}
    """Model results for the image."""

    @field_validator('metadata', mode='before')
    @classmethod
    def parse_metadata(cls, v: Any) -> BenchmarkMetadata:
        """Our metadata field is stored as a JSON string, so we need to
        parse it into a dictionary."""
        if isinstance(v, str):
            try:
                data = BenchmarkMetadata.model_validate_json(v)
            except json.JSONDecodeError as e:
                raise ValueError(f"Invalid JSON for nested field: {e}") from e
            return data
        return v
    
    @field_validator('true_markdown_output', mode='before')
    @classmethod
    def parse_true_markdown_output(cls, v: Any) -> str:
        """Our true_markdown_output field is stored as a JSON string
        inside a string, so we need to parse it."""
        if isinstance(v, str):
            try:
                data = json.loads(v)
                if not isinstance(data, str):
                    raise ValueError("true_markdown_output must be a string")
            except json.JSONDecodeError as e:
                raise ValueError(f"Invalid JSON for nested field: {e}") from e
            return data
        return v
    
    @staticmethod
    def load_all() -> List[BenchmarkImage]:
        """Load our benchmark metadata."""
        return list(
            jsonl_records(BenchmarkImage, OCR_BENCHMARK_TEST_PATH / "metadata.jsonl")
        )
    
    def path(self) -> Path:
        """Get the path to the image file."""
        return OCR_BENCHMARK_TEST_PATH / self.file_name
    
    def rel_path(self) -> Path:
        """Get the relative path to the image file."""
        return Path(self.path()).relative_to(OCR_BENCHMARK_PATH)
    
    def add_model_results(
        self, model_name: str, output: OcrOutput
    ) -> None:
        """Add the model results to this image."""
        if self.id != int(output.id):
            raise ValueError(
                f"Model output ID {output.id} does not match image ID {self.id}"
            )
        self_tokens = DocTokens.from_text(self.true_markdown_output)
        extracted_markdown = output.text or ""
        model_tokens = DocTokens.from_text(extracted_markdown)
        similarity = self_tokens.jaqqard(model_tokens)
        token_diff = self_tokens.diff(model_tokens)
        highlighted_markdown = token_diff.highlight_markdown(extracted_markdown)
        self.model_results[model_name] = ModelResults(
            output=output,
            jaqqard_similarity=similarity,
            extracted_markdown=highlighted_markdown,
            missing_tokens=token_diff.removed,
        )

class ModelResults(BaseModel):
    """Results of performaing OCR on this image with the specified model."""

    output: OcrOutput
    """OCR output for the image."""

    jaqqard_similarity: float
    """Jaqqard similarity score for the extracted text."""

    extracted_markdown: str
    """Extracted Markdown text from the image."""

    missing_tokens: Set[str]
    """Set of tokens that are missing from the extracted text."""

    def result_is_flagged(self) -> bool:
        """Check if this model result is flagged as bad, without checking
        it against the ground truth."""
        if self.output.analysis is None:
            return False
        a = self.output.analysis
        return (
            #a.background_is_noisy
            a.contains_blurred_text or
            a.contains_cutoff_text or
            a.contains_distorted_text or
            a.contains_faint_text or
            a.contains_handwriting or
            a.contains_unreadable_or_ambiguous_text or
            a.glare_on_some_text
        )
