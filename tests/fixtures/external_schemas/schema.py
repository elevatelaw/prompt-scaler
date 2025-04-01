import json
from pathlib import Path
from pydantic import BaseModel, Field

class JokeResponse(BaseModel):
    """Response to a joke."""

    # You need to pass extra="forbid" or OpenAI will not accept the schema.
    model_config = dict(extra="forbid")

    punchline: str = Field(description="The punchline of the joke.")

# Write the schema to schema.json in the same directory
# as this file.
main_model_schema = JokeResponse.model_json_schema()
with open(Path(__file__).parent / "schema_py.json", "w") as f:
    f.write(json.dumps(main_model_schema, indent=2))


