-- thurbox-voice: dictate into the selected session.
--
-- `ctrl+space` starts recording for the session that is selected *now*. In the
-- default `toggle` mode the next `ctrl+space` stops; in `hold` mode letting go
-- of it does. The `thurbox-voice` daemon then transcribes, has an LLM fix
-- misheard words using that session's context, and pastes the text into its
-- composer — never submitted, so you read it and press Enter.
--
-- The session, the speech model and the mode are captured when a recording
-- starts: switching session or changing a setting mid-sentence changes the
-- next dictation, never this one.
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
local MODES = { "toggle", "hold" }

-- What each engine id runs, for the strip. The speech model only: the
-- cleanup model is the daemon's `[cleanup]` config, and `stop` reports it.
local MODELS = {
  parakeet = "Parakeet TDT 0.6B v3",
  whisper = "Whisper large-v3-turbo",
}

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

local function model(id)
  return MODELS[id] or id
end

--- Whether this thurbox can tell us the chord was let go. A thurbox from
--- before key releases publishes no `keyboard` at all.
local function releases_reported()
  local keyboard = thurbox and thurbox.keyboard
  return keyboard ~= nil and keyboard.releases ~= nil and keyboard.releases ~= "unsupported"
end

--- `hold` only where a release can arrive; anywhere else it would have no way
--- to stop but a second press, which is `toggle`.
local function holding()
  return plugin_settings.get(NAME, "mode", "toggle") == "hold" and releases_reported()
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
    state.pending = nil
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
    -- A stop or cancel asked for while the start was in flight goes now, after
    -- it: sent alongside, it could reach the daemon first and leave the
    -- microphone on.
    local pending = state.pending
    state.pending = nil
    if pending == "stop" then
      state.phase = "stopping"
      queue("stop", BIN .. " stop")
    elseif pending == "cancel" then
      state.phase = "cancelling"
      queue("cancel", BIN .. " cancel")
    end
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
  local recorded = model(state.engine or engine())
  if state.phase == "recording" then
    state.since = state.since or now
    local how = state.hold and ("release " .. key .. " to stop") or (key .. " to stop")
    return {
      span(" ● REC ", theme.bad, true),
      span(clock(now - state.since), theme.bad, true),
      span("  → " .. name, theme.text),
      span("  ·  " .. recorded, theme.muted),
      span("  ·  " .. how, theme.hint),
    }
  end
  if state.phase == "starting" then
    return { span(" ◌ starting the microphone…  ·  " .. recorded, theme.muted) }
  end
  if state.phase == "stopping" then
    return {
      span(" ⋯ transcribing with " .. recorded .. " for " .. name .. "…", theme.warn),
    }
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
  local target = session and ("  → " .. (session.name or session.id)) or "  (select a session)"
  local idle = { span(" ○ ", theme.muted) }
  if holding() then
    idle[#idle + 1] = span("hold " .. key, theme.hint, true)
    idle[#idle + 1] = span(" to talk", theme.muted)
  else
    idle[#idle + 1] = span(key, theme.hint, true)
    idle[#idle + 1] = span(" to start talking", theme.muted)
  end
  idle[#idle + 1] = span(target, theme.muted)
  idle[#idle + 1] = span("  ·  " .. model(engine()), theme.muted)
  if plugin_settings.get(NAME, "mode", "toggle") == "hold" and not holding() then
    idle[#idle + 1] = span(
      "  ·  hold unavailable: this terminal reports no key releases — press to start, press to stop",
      theme.warn
    )
  end
  return idle
end

return {
  name = NAME,
  -- A strip two rows high: this line, then a gap above the bars. thurbox
  -- places strips by itself (`ctx.strips`); a layout.lua from before strips
  -- needs `{ slot = "voice", len = 2 }` added instead.
  slot = "voice",
  strip = true,
  size = { len = 2 },
  focusable = false,
  capabilities = { "run" },

  -- `choices` makes each a picker in settings (F6); a thurbox from before
  -- choices ignores the field and shows a text field, which still works.
  settings = {
    {
      id = "engine",
      desc = "Speech recognition model: parakeet (Parakeet TDT 0.6B v3) or whisper (Whisper large-v3-turbo); not the cleanup model",
      default = "parakeet",
      choices = ENGINES,
    },
    {
      id = "mode",
      desc = "Recording: toggle (press to start, press to stop) or hold (record while held; needs key releases)",
      default = "toggle",
      choices = MODES,
    },
  },

  keys = {
    {
      key = "ctrl+space",
      action = TOGGLE,
      desc = "dictate into the selected session (press again, or let go in hold mode, to stop)",
      scope = "global",
      group = "Voice",
      -- Asks for the release as well, and keeps auto-repeat from toggling.
      release = true,
    },
  },

  commands = {
    { action = CANCEL, desc = "voice: cancel the recording" },
    { action = SWITCH, desc = "voice: switch speech recognition model (parakeet / whisper)" },
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

  -- `args.event` is "press" or "release" from a thurbox that reports key
  -- releases; anything else (an older thurbox, the palette) is a press.
  on_action = function(action, args)
    if action == TOGGLE then
      local event = args and args.event or "press"
      if event == "release" then
        -- Only a hold ends on a release; a toggle's own release is noise.
        if state.hold and state.phase == "starting" then
          state.pending = "stop"
        elseif state.hold and state.phase == "recording" then
          state.phase = "stopping"
          queue("stop", BIN .. " stop")
        end
        return true
      end
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
        state.pending = nil
        state.session_name = session.name or session.id
        state.engine = engine()
        state.hold = holding()
        queue(
          "start",
          BIN .. " start --session " .. quote(session.id) .. " --engine " .. quote(state.engine)
        )
      elseif state.phase == "starting" then
        -- A second press always stops, in either mode: it is also what ends a
        -- hold whose release never arrived.
        state.pending = "stop"
      elseif state.phase == "recording" then
        state.phase = "stopping"
        queue("stop", BIN .. " stop")
      end
      return true
    end

    if action == CANCEL then
      if state.phase == "starting" then
        state.pending = "cancel"
      elseif state.phase == "recording" then
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
      outcome("speech model → " .. model(nextone) .. " (from the next dictation)", "muted")
      return true
    end

    return false
  end,
}
