# generated by datamodel-codegen:
#   filename:  tmp_schema.json
#   timestamp: 2025-05-03T19:36:11+00:00

from __future__ import annotations

from enum import Enum
from typing import Annotated, Any, List, Optional

from pydantic import BaseModel, ConfigDict, Field


class ImageSource(str, Enum):
    PHOTO_OR_VIDEO = 'PHOTO_OR_VIDEO'
    SCAN = 'SCAN'
    DIGITAL = 'DIGITAL'


class OcrAnalysis(BaseModel):
    model_config = ConfigDict(
        extra='forbid',
    )
    background_is_noisy: Annotated[
        bool, Field(description='The background behind the text is noisy.')
    ]
    contains_blurred_text: Annotated[
        bool,
        Field(description='The document contains text is blurred or out of focus.'),
    ]
    contains_cutoff_text: Annotated[
        bool, Field(description='The document contains text that is cut off.')
    ]
    contains_distorted_text: Annotated[
        bool,
        Field(
            description='The document contains distorted text, including from crinkled paper, perspective distortion, or other artifacts.'
        ),
    ]
    contains_faint_text: Annotated[
        bool,
        Field(description='The document contains text that is faint or low-contrast.'),
    ]
    contains_handwriting: Annotated[
        bool, Field(description='The document contains handwriting.')
    ]
    contains_unreadable_or_ambiguous_text: Annotated[
        bool,
        Field(
            description='The document contains text that may not have been OCRed correctly.'
        ),
    ]
    glare_on_some_text: Annotated[
        bool, Field(description='The image contains glare obscuring the text.')
    ]
    image_source: Annotated[ImageSource, Field(description='The source of this image.')]


class TokenUsage(BaseModel):
    completion_tokens: Annotated[
        int, Field(description='How many tokens were used in the response?', ge=0)
    ]
    prompt_tokens: Annotated[
        int, Field(description='How many tokens were used in the prompt?', ge=0)
    ]


class WorkStatus(str, Enum):
    ok = 'ok'
    incomplete = 'incomplete'
    failed = 'failed'


class OcrOutput(BaseModel):
    analysis: Annotated[
        Optional[OcrAnalysis],
        Field(description='Any defects in the page that make it difficult to OCR.'),
    ] = None
    errors: Annotated[
        Optional[List[str]],
        Field(description='Any errors that occurred during processing.'),
    ] = None
    estimated_cost: Annotated[
        Optional[float], Field(description='How much money do we think we spent?')
    ] = None
    id: Annotated[Any, Field(description='The unique ID of the work item.')]
    path: Annotated[str, Field(description='The input path.')]
    status: Annotated[
        WorkStatus, Field(description='What is the status of this work item?')
    ]
    text: Annotated[
        Optional[str],
        Field(
            description='The text extracted from the PDF. If errors occur on specific pages, those pages will be replaced with `**COULD_NOT_OCR_PAGE**`.'
        ),
    ] = None
    token_usage: Annotated[
        Optional[TokenUsage], Field(description='How many tokens did we use?')
    ] = None
