-- Add fields to persist tool call/result data alongside messages so that
-- conversation history can faithfully reconstruct the tool interaction
-- sequence on reload.

-- For role='tool' messages: the ID of the tool call this result responds to.
ALTER TABLE messages ADD COLUMN tool_call_id TEXT;

-- For role='assistant' messages: JSON array of tool calls requested by the LLM.
-- Stored as serialized Vec<ToolCall> so we can reconstruct
-- ChatMessage::assistant_with_tool_calls on context reload.
ALTER TABLE messages ADD COLUMN tool_calls_json TEXT;
