"""Filters for Jinja2 templates."""

import json
from re import Match
from typing import Any

import bleach
import markdown
from markdown.inlinepatterns import InlineProcessor
from markdown.extensions import Extension
from markupsafe import Markup
from pydantic import BaseModel
import xml.etree.ElementTree as etree


class SpanClassInlineProcessor(InlineProcessor):
    def handleMatch(self, m: Match[str], data: str):
        el = etree.Element('span')
        el.attrib['class'] = m.group(1)
        el.text = m.group(2)
        return el, m.start(0), m.end(0)

class SpanClassExtension(Extension):
    def extendMarkdown(self, md: markdown.Markdown) -> None:
        SPAN_PATTERN = r'<span\s+class="([^"]*)">(.*?)</span>'
        md.inlinePatterns.register(SpanClassInlineProcessor(SPAN_PATTERN, md), 'span', 200)

def markdown_filter(value: Any) -> Markup:
    if value is None:
        return Markup('')

    md = markdown.Markdown(
        extensions=['extra', 'sane_lists'],
        output_format='html5'
    )
    SpanClassExtension().extendMarkdown(md)
    html = md.convert(str(value))

    cleaned = bleach.clean(
        html,
        tags=[*bleach.sanitizer.ALLOWED_TAGS, "h1", "h2", "h3", "h4", "h5", "h6",
              "p", "table", "thead", "tbody", "tr", "td", "th", "hr", "br", "page_number",
              "span"],
        attributes={ **bleach.sanitizer.ALLOWED_ATTRIBUTES, "span": ["class"] },        
    )

    return Markup(cleaned)

def json_filter(value: Any) -> str:
    """Convert a Python object to a JSON string."""
    if isinstance(value, BaseModel):
        # If it's a Pydantic model, use model_dump_json to make it pretty.
        return value.model_dump_json(indent=2)
    else:
        # Assume it's Plain Old Data.
        return json.dumps(value, indent=2)
    