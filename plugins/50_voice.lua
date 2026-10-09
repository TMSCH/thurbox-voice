-- thurbox-voice: dictate into the selected session.
--
-- `ctrl+space` starts recording for the session that is selected *now*; the
-- next `ctrl+space` stops, and the `thurbox-voice` daemon transcribes, has an
-- LLM fix misheard words using that session's context, and pastes the text
-- into its composer — never submitted, so you read it and press Enter.
--
-- A one-line strip that is always there, in the `voice` slot, so nothing on
-- screen moves when a recording starts or ends: idle, it says how to start;
-- recording, it counts; afterwards it says what happened for a few seconds and
-- goes back to idle. A blank row under it keeps it off the bars below.
--
-- Every program runs with `machine = "local"`: the microphone is on the machine
-- running thurbox, whichever host the selected session lives on.

local plugin_settings = require("lib.settings")
local theme = require("lib.theme")

local NAME = "voice"
local BIN = "thurbox-voice"

local TOGGLE = "voice.toggle"
local CANCEL = "voice.cancel"
local SWITCH = "voice.engine"

local ENGINES = { "parakeet", "whisper" }

-- How long an outcome stays on the strip before it returns to idle.
local OUTCOME_SECONDS = 6

-- `stop` transcribes, cleans up and pastes; an agent-CLI cleanup alone can take
-- several seconds, so allow far more than `run`'s 30 s default.
local STOP_TIMEOUT = 120

--- The session the list has selected, from the current snapshot.
local function selected()
  local id = store.selected
  if not id then
    return nil
  end
  for _, session in ipairs(thurbox and thurbox.sessions or {}) do
    if session.id == id then
      return session
    end
  end
  return nil
end

local function engine()
  return plugin_settings.get(NAME, "engine", "parakeet")
end

--- The chord the toggle is bound to now, so a rebinding shows here too.
local function chord()
  local registry = thurbox and thurbox.registry
  for _, entry in ipairs(registry and registry.keys or {}) do
    if entry.action == TOGGLE then
      return entry.key
    end
  end
  return "ctrl+space"
end

--- Single-quoted for `sh -c`. A session id is a UUID, but quoting costs
--- nothing and a hand-made id is not ours to trust.
local function quote(text)
  return "'" .. tostring(text):gsub("'", "'\\''") .. "'"
end

--- Queue one program; `render` starts it, since that is where `run` is asked
--- for. A fresh key per request, so a second `stop` is a new run rather than
--- the cached answer of the first.
local function queue(verb, program)
  state.serial = (state.serial or 0) + 1
  state.ask = { key = verb .. "." .. state.serial, verb = verb, program = program }
end

--- What the strip says until it is told something newer, and for how long.
local function outcome(text, kind)
  state.outcome = { text = text, kind = kind }
  state.outcome_at = nil
end

local function clock(seconds)
  seconds = math.max(0, math.floor(seconds or 0))
  return string.format("%d:%02d", math.floor(seconds / 60), seconds % 60)
end

local function first_line(text)
  local line = (text or ""):match("[^\n]+") or ""
  return (line:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function settle(ask, answer)
  local out = first_line(answer.stdout)
  local err = first_line(answer.stderr)
  if answer.state == "failed" or not answer.ok then
    state.phase = nil
    local why = err ~= "" and err or (answer.error or "thurbox-voice failed")
    if answer.timed_out then
      why = "thurbox-voice timed out"
    end
    outcome((why:gsub("^Error: ", "")), "error")
    return
  end
  if ask.verb == "start" then
    state.phase = "recording"
    state.since = nil
    return
  end
  state.phase = nil
  local target = state.session_name or "session"
  if ask.verb == "stop" then
    if out:match("^dictated") then
      outcome(out .. " → " .. target .. " — review, then Enter", "ok")
    else
      outcome(out ~= "" and out or "nothing transcribed", "muted")
    end
  elseif ask.verb == "cancel" then
    outcome("recording discarded", "muted")
  end
end

local function span(text, fg, bold)
  return { text = text, style = { fg = fg, bold = bold } }
end

--- The strip's one line, for the state we are in.
local function line(now)
  local name = state.session_name or "session"
  local key = chord()
  if state.phase == "recording" then
    state.since = state.since or now
    return {
      span(" ● REC ", theme.bad, true),
      span(clock(now - state.since), theme.bad, true),
      span("  → " .. name, theme.text),
      span("  ·  " .. engine(), theme.muted),
      span("  ·  " .. key .. " to stop", theme.hint),
    }
  end
  if state.phase == "starting" then
    return { span(" ◌ starting the microphone…", theme.muted) }
  end
  if state.phase == "stopping" then
    return { span(" ⋯ transcribing for " .. name .. "…", theme.warn) }
  end
  if state.phase == "cancelling" then
    return { span(" ◌ discarding…", theme.muted) }
  end

  local shown = state.outcome
  if shown then
    state.outcome_at = state.outcome_at or now
    if now - state.outcome_at < OUTCOME_SECONDS then
      local fg = shown.kind == "ok" and theme.ok
        or shown.kind == "error" and theme.bad
        or theme.muted
      local mark = shown.kind == "ok" and " ✓ " or shown.kind == "error" and " ✗ " or " · "
      return { span(mark, fg, true), span(shown.text, fg) }
    end
    state.outcome = nil
  end

  if not run then
    return {
      span(" ○ ", theme.muted),
      span("voice needs trust: Ctrl+, then ] then t on voice", theme.muted),
    }
  end
  local session = selected()
  return {
    span(" ○ ", theme.muted),
    span(key, theme.hint, true),
    span(" to start talking", theme.muted),
    span(session and ("  → " .. (session.name or session.id)) or "  (select a session)", theme.muted),
  }
end

return {
  name = NAME,
  -- Placed by `layout.lua` as a strip two rows high: this line, then a gap.
  slot = "voice",
  focusable = false,
  capabilities = { "run" },

  settings = {
    { id = "engine", desc = "Speech engine: parakeet or whisper", default = "parakeet" },
  },

  keys = {
    {
      key = "ctrl+space",
      action = TOGGLE,
      desc = "dictate into the selected session (press again to stop)",
      scope = "global",
      group = "Voice",
    },
  },

  commands = {
    { action = CANCEL, desc = "voice: cancel the recording" },
    { action = SWITCH, desc = "voice: switch speech engine (parakeet / whisper)" },
  },

  render = function(ctx)
    local now = ctx.elapsed or 0

    local ask = state.ask
    if ask and run then
      run(ask.key, ask.program, {
        machine = "local",
        timeout = ask.verb == "stop" and STOP_TIMEOUT or 15,
        -- An answer is consumed once; a long TTL keeps it from being re-run
        -- before it is.
        ttl = 3600,
      })
      local answer = (thurbox.runs or {})[ask.key]
      if answer and answer.state ~= "pending" then
        state.ask = nil
        settle(ask, answer)
      end
    end

    return { type = "text", text = { line(now) } }
  end,

  on_action = function(action)
    if action == TOGGLE then
      if not run then
        outcome("trust this plugin first: Ctrl+, then ] then t on voice", "error")
        return true
      end
      if state.phase == nil then
        local session = selected()
        if not session then
          outcome("select a session to dictate into", "error")
          return true
        end
        state.phase = "starting"
        state.outcome = nil
        state.session_name = session.name or session.id
        queue(
          "start",
          BIN .. " start --session " .. quote(session.id) .. " --engine " .. quote(engine())
        )
      elseif state.phase == "starting" or state.phase == "recording" then
        state.phase = "stopping"
        queue("stop", BIN .. " stop")
      end
      return true
    end

    if action == CANCEL then
      if state.phase == "recording" or state.phase == "starting" then
        state.phase = "cancelling"
        queue("cancel", BIN .. " cancel")
      else
        outcome("not recording", "muted")
      end
      return true
    end

    if action == SWITCH then
      local current, nextone = engine(), ENGINES[1]
      for i, name in ipairs(ENGINES) do
        if name == current then
          nextone = ENGINES[(i % #ENGINES) + 1]
        end
      end
      command("set", { text = NAME .. ".engine", value = nextone })
      outcome("engine → " .. nextone .. " (from the next dictation)", "muted")
      return true
    end

    return false
  end,
}
