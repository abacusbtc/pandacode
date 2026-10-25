#!/usr/bin/env python3
"""A stand-in harness CLI, so the gateway's turn machinery can be driven for real
without spending a model quota.

The gateway spawns a provider CLI as a child process and speaks the protocol on its
stdin/stdout. Everything above that boundary — the turn lifecycle, the live registry,
the event ring, approvals, the transcript, the audit — is the gateway's own code. This
script replaces only the child, so a test can exercise all of that with the real binary,
a real socket and a real `kill -9`, and no network.

It speaks the frames the app's Claude protocol already parses (the shapes are pinned in
`src-tauri/src/providers/claude_stream_tests.rs`): an `init` system frame, an optional
`can_use_tool` control request, streaming text deltas, the final assistant message, and a
`result` frame that ends the turn.

Behaviour is chosen by the `--model` id the session was created with, so one shared
executable on PATH serves every test and no test has to touch the process environment:

    --model fake:stream:10:200     ten text deltas, 200 ms apart
    --model fake:stream:700:0:4000 seven hundred deltas of ~4 KiB each, as fast as the
                                   pipe takes them (a real ring overflow needs volume,
                                   not just count: a socket buffer swallows small frames)
    --model fake:approval:<path>   raise a can_use_tool approval first, then stream;
                                   append the decision this child was given to <path>
    --model fake:hold              one delta, then hold the turn open until stdin
                                   closes (a mid-turn kill, a dropped client)
    --model fake:report:<path>     append the model this child was launched with to
                                   <path>, then stream (PAN-159: the turn reports the
                                   model the session was configured with)

The environment is a fallback for running it by hand: FAKE_DELTAS, FAKE_DELAY_MS,
FAKE_APPROVAL, FAKE_LOG, FAKE_HOLD, FAKE_TEXT.
"""
import json
import os
import sys
import time


def emit(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def read_stdin_line():
    line = sys.stdin.readline()
    if not line:
        return None
    try:
        return json.loads(line)
    except Exception:
        return {}


def parse_model(argv):
    """`fake:<verb>[:<arg>[:<arg>]]` -> (verb, args). Anything else is ignored."""
    value = next_model(argv)
    if not value or not value.startswith("fake:"):
        return None
    parts = value.split(":")
    return parts[1], parts[2:]


def next_model(argv):
    """The `--model` value this child was launched with, however the caller spelled it."""
    value = None
    for index, argument in enumerate(argv):
        if argument == "--model" and index + 1 < len(argv):
            value = argv[index + 1]
        elif argument.startswith("--model="):
            value = argument.split("=", 1)[1]
    return value


def main():
    plan = parse_model(sys.argv[1:])
    verb, args = plan if plan else ("stream", [])
    text = os.environ.get("FAKE_TEXT", "fake harness says hello")
    deltas = int(os.environ.get("FAKE_DELTAS", "3"))
    delay_ms = int(os.environ.get("FAKE_DELAY_MS", "0"))
    log = os.environ.get("FAKE_LOG")
    pad = 0
    if verb == "stream" and args:
        deltas = int(args[0])
        delay_ms = int(args[1]) if len(args) > 1 else 0
        pad = int(args[2]) if len(args) > 2 else 0
    if verb == "approval" and args:
        log = args[0]
    if verb == "report" and args:
        # PAN-159: say which model this child was actually launched with. A test that
        # configures a session and then runs a turn can prove the configured model
        # reached the child, rather than only that the session row changed.
        with open(args[0], "a") as handle:
            handle.write(json.dumps({"model": next_model(sys.argv[1:])}) + "\n")
    hold = verb == "hold" or "hold" in args or os.environ.get("FAKE_HOLD") == "1"

    # The model this child was started with, so the frame is honest rather than a constant.
    emit({"type": "system", "subtype": "init", "session_id": "fake-session", "model": next_model(sys.argv[1:])})

    # The app sends the prompt as one JSON line on stdin. Reading it is the whole
    # handshake: a harness that ignored stdin would deadlock the app's writer.
    prompt = read_stdin_line()
    del prompt  # never logged, never echoed: this child is not a model

    if verb == "approval":
        request_id = "fake-approval-1"
        emit({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "echo hello"}},
        })
        decision = None
        while True:
            frame = read_stdin_line()
            if frame is None:
                return 3
            response = frame.get("response") or {}
            if frame.get("type") == "control_response" and response.get("request_id") == request_id:
                decision = (response.get("response") or {}).get("behavior", "unknown")
                break
        if log:
            with open(log, "a") as handle:
                handle.write(json.dumps({"behavior": decision, "requestId": request_id}) + "\n")
        if decision != "allow":
            # A denial is a refusal the agent can plan around: say so and finish.
            emit({"type": "assistant", "message": {"id": "m0", "content": [{"type": "text", "text": "Declined as asked."}]}})
            emit({"type": "result", "subtype": "success", "is_error": False, "result": "Declined as asked.", "session_id": "fake-session", "num_turns": 1})
            return 0

    emit({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "main"}}})
    for index in range(deltas):
        emit({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": f"{text} [{index + 1}] " + "x" * pad},
            },
        })
        if delay_ms:
            time.sleep(delay_ms / 1000.0)

    if hold:
        # Mid-turn, for real: the parent waits for the next frame while this child sits
        # on its stdin. When the gateway dies its end of the pipe closes, so this exits
        # by itself rather than orphaning a process.
        while read_stdin_line() is not None:
            pass
        return 0

    full = "".join(f"{text} [{index + 1}] " + "x" * pad for index in range(deltas))
    emit({"type": "assistant", "message": {"id": "main", "content": [{"type": "text", "text": full}]}})
    emit({
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": full,
        "session_id": "fake-session",
        "num_turns": 1,
        "total_cost_usd": 0.0,
        "usage": {"input_tokens": 11, "output_tokens": 22},
    })
    return 0


if __name__ == "__main__":
    sys.exit(main())
