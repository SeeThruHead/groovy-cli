#!/bin/bash
# Ralph Loop - autonomous refactor of groovy-cli
# Fresh context each iteration, state in files/git, loop until done
cd /Users/sth/code/groovy-cli

while true; do
    READY=$(tk ready 2>/dev/null | grep -c "open\|in_progress" || true)
    if [ "$READY" -eq 0 ]; then
        echo "$(date): ALL TICKETS DONE!" >> ralph.log
        break
    fi

    echo "$(date): $READY tickets remaining." >> ralph.log
    TICKET=$(tk ready 2>/dev/null | head -1 | awk '{print $1}')
    if [ -z "$TICKET" ]; then
        echo "$(date): No ready tickets, sleeping..." >> ralph.log
        sleep 10
        continue
    fi

    echo "$(date): Working on $TICKET" >> ralph.log
    tk start "$TICKET" 2>/dev/null || true
    TICKET_DETAIL=$(tk show "$TICKET" 2>/dev/null)
    ARCH=$(cat ARCHITECTURE.md 2>/dev/null)

    # Build the prompt file
    cat > /tmp/ralph-prompt.md << PROMPT
You are refactoring groovy-cli (Rust CLI for streaming Plex to MiSTer FPGA via Groovy protocol).

Working directory: /Users/sth/code/groovy-cli

CURRENT TICKET:
$TICKET_DETAIL

ARCHITECTURE:
$ARCH

RULES:
- Read the existing src/ files to understand current state.
- Write the code for this ticket. Write tests. Make cargo test pass. Make cargo build --release pass.
- Do NOT ask questions. Do NOT wait for input. Just do the work.
- Do NOT commit to git. Do NOT push. Just write code and tests.
- Keep existing CLI functionality working.
- If this ticket depends on other modules, read them from src/ - they should already exist.
- Suppress warnings with appropriate allows if needed, but prefer fixing them.

When done, run these commands to validate:
  cargo test
  cargo build --release
  cargo check 2>&1 | grep -c warning

Fix any issues until all pass.
PROMPT

    # Run pi in non-interactive mode (-p flag)
    echo "$(date): Running pi for $TICKET" >> ralph.log
    pi -p @/tmp/ralph-prompt.md >> ralph.log 2>&1

    # Validate
    echo "$(date): Validating $TICKET" >> ralph.log
    if cargo test >> ralph.log 2>&1 && cargo build --release >> ralph.log 2>&1; then
        echo "$(date): $TICKET PASSED" >> ralph.log
        tk close "$TICKET" 2>/dev/null || true
        git add -A 2>/dev/null
        git commit -m "refactor: $(tk show $TICKET 2>/dev/null | head -1 | sed 's/^[^ ]* //')" 2>/dev/null || true
    else
        echo "$(date): $TICKET FAILED - will retry" >> ralph.log
        tk status "$TICKET" open 2>/dev/null || true
    fi

    echo "$(date): --- iteration done ---" >> ralph.log
done

echo "$(date): Ralph loop complete." >> ralph.log
