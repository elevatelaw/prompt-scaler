"""
Compare two documents using Jaqqard similarity.

This is set up in a way that makes sense for OCR use cases, where
we are interested in how closely the text tokens in two documents
agree. We omit punctuation, and we completely ignore token order
(at this level).
"""

from __future__ import annotations
from dataclasses import dataclass
import re

MD_IMG_REGEX = re.compile(r"!\[.*?\]\(.*?\)", re.MULTILINE)

def text_to_tokens(text: str) -> list[str]:
    """Convert text to tokens, stripping HTML tags.""" 
    no_tags = re.sub(r"</?[a-zA-z_]+\s*/?>", " ", text)
    no_tags_no_img = re.sub(MD_IMG_REGEX, " ", no_tags)
    return [ t.lower() for t in re.findall(r"\b\w+\b", no_tags_no_img) if not re.match(r"^_+$", t) ]

def text_to_token_counts(text: str) -> dict[str, int]:
    """Convert text to token counts."""
    tokens = text_to_tokens(text)
    token_counts = {}
    for token in tokens:
        token_counts[token] = token_counts.get(token, 0) + 1
    return token_counts

class DocTokens:
    token_counts: dict[str, int]

    def __init__(self, token_counts: dict[str, int]):
        """Initialize DocTokens with a dictionary of token counts."""
        assert isinstance(token_counts, dict)
        self.token_counts = token_counts

    @staticmethod
    def from_text(text: str) -> DocTokens:
        """Create a DocTokens object from text."""
        return DocTokens(text_to_token_counts(text))

    def jaqqard(self, other: DocTokens) -> float:
        """Compute the Jaqqard similarity between two token counts."""
        intersection = 0
        union = 0
        all_tokens = set(self.token_counts.keys()).union(other.token_counts.keys())
        for token in all_tokens:
            count1 = self.token_counts.get(token, 0)
            count2 = other.token_counts.get(token, 0)
            intersection += min(count1, count2)
            union += max(count1, count2)
        if union == 0:
            # Treat two empty documents as identical.
            return 1.0
        return 1.0 * intersection / union
    
    def diff(self, other: DocTokens) -> TokenDiff:
        """Compute how tokens have changed between two documents."""
        removed = set()
        changed = set()
        added = set()

        all_tokens = set(self.token_counts.keys()).union(other.token_counts.keys())
        for token in all_tokens:
            count1 = self.token_counts.get(token, 0)
            count2 = other.token_counts.get(token, 0)
            if count1 == 0 and count2 > 0:
                added.add(token)
            elif count1 > 0 and count2 == 0:
                removed.add(token)
            elif count1 != count2:
                changed.add(token)
        return TokenDiff(removed=removed, changed=changed, added=added)

@dataclass
class TokenDiff:
    """Tokens which differ between two documents."""

    removed: set[str]
    """Tokens which were removed from the first document."""

    changed: set[str]
    """Tokens which were exist in both documents, but with different counts."""

    added: set[str]
    """Tokens which were added to the second document."""

    def highlight_markdown(self, markdown: str) -> str:
        """Highlight how this document differs from a base document.
        
        We return Markdown with <span class="added"> and <span class="changed">.
        For removed tokens, use self.removed directly.
        """
        for token in self.changed:
            markdown = re.sub(rf"\b{token}\b", rf'<span class="changed">{token}</span>', markdown, flags=re.IGNORECASE)
        for token in self.added:
            markdown = re.sub(rf"\b{token}\b", rf'<span class="added">{token}</span>', markdown, flags=re.IGNORECASE)
        return markdown