"""Utility functions for working with JSONL files."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Iterable, List, Type, TypeVar

from pydantic import BaseModel

T = TypeVar('T', bound='BaseModel')

@staticmethod
def jsonl_records(cls: Type[T], path: Path) -> Iterable[T]:
    """Iterate over JSONL records."""

    # Read JSONL and deserialize using Pydantic.
    with open(path, "r") as f:
        for line in f:
            try:
                yield cls.model_validate_json(line)
            except json.JSONDecodeError as e:
                print(f"Failed to parse line: {line.strip()}")
                raise e
