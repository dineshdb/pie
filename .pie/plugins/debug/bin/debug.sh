#!/bin/bash
# PIE Debug Plugin Script
# Writes hook inputs to ~/.pie/debug/<session_id>/<timestamp>_<event>.json

DEBUG_ROOT="${HOME}/.pie/debug"
SESSION_DIR="${DEBUG_ROOT}/${PIE_SESSION_ID}"
mkdir -p "${SESSION_DIR}"

# Use nanoseconds for unique filenames in rapid successions (like parallel hooks)
TIMESTAMP=$(date +%s%N)
# Clean event name for filename (e.g., prompt.post -> prompt_post)
CLEAN_EVENT=$(echo "${PIE_EVENT}" | tr '.' '_')

FILENAME="${TIMESTAMP}_${CLEAN_EVENT}.json"

# Capture the input JSON
if [ -n "${PIE_INPUT}" ]; then
  echo "${PIE_INPUT}" > "${SESSION_DIR}/${FILENAME}"
  
  # For prompt events, also write a human-readable Markdown file
  if [[ "${PIE_EVENT}" == "prompt.pre" || "${PIE_EVENT}" == "prompt.post" ]]; then
    MD_FILENAME="${TIMESTAMP}_${CLEAN_EVENT}.md"
    {
      echo "# Debug Event: ${PIE_EVENT}"
      echo "Timestamp: $(date)"
      echo "Session ID: ${PIE_SESSION_ID}"
      echo
      
      # Use jq to extract system and query if they exist
      SYSTEM=$(echo "${PIE_INPUT}" | jq -r '.system // empty')
      QUERY=$(echo "${PIE_INPUT}" | jq -r '.query // empty')
      
      if [ -n "${SYSTEM}" ]; then
        echo "---"
        echo "<!-- SYSTEM PROMPT -->"
        echo "## System Prompt"
        echo '```markdown'
        echo "${SYSTEM}"
        echo '```'
        echo
      fi
      
      if [ -n "${QUERY}" ]; then
        echo "---"
        echo "<!-- USER QUERY -->"
        echo "## User Query"
        echo '```markdown'
        echo "${QUERY}"
        echo '```'
      fi
    } > "${SESSION_DIR}/${MD_FILENAME}"
  fi
else
  # Fallback to reading from stdin if PIE_INPUT is empty and it's an action
  cat > "${SESSION_DIR}/${FILENAME}"
fi
