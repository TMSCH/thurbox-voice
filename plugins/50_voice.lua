-- thurbox-voice: dictate into the selected session.
--
-- `ctrl+space` starts recording for the session that is selected *now*; the
-- next `ctrl+space` stops, and the `thurbox-voice` daemon transcribes, has an
-- LLM fix misheard words using that session's context, and pastes the text
-- into its composer — never submitted, so you read it and press Enter.
--
-- This pane draws nothing of its own. It only floats (a slot nothing places),
-- because floats render on every frame, which is what keeps the recording
-- message in the band refreshed while you talk.
--
-- Every program runs with `machine = "local"`: the microphone is on the machine
-- running thurbox, whichever host the selected session lives on.

local plugin_settings = require("lib.settings")

local NAME = "voice"
local BIN = "thurbox-voice"

local TOGGLE = "voice.toggle"
local CANCEL = "voice.cancel"
local SWITCH = "voice.engine"

local ENGINES = { "parakeet", "whisper" }

-- Seconds between refreshes of the recording message. The band drops a
-- message after five, so anything under that keeps it up without flicker.
local REFRESH = 1

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

--- Single-quoted for `sh -c`. A session id is a UUID, but quoting costs
--- nothing and a hand-made id is not ours to trust.
local function quote(text)
  return "'" .. tostring(text):gsub("'", "'\\''") .. "'"
end

local function say(text, level)
  command("message", { text = text, level = level })
end

--- Queue one program; `render` starts it, since that is where `run` is asked
--- for. A fresh key per request, so a second `stop` is a new run rather than
--- the cached answer of the first.
local function queue(verb, program)
  state.serial = (state.serial or 0) + 1
  state.ask = { key = verb .. "." .. state.serial, verb = verb, program = program }
end

local function clock(seconds)
  seconds = math.max(0, math.floor(seconds or 0))
  return string.format("%d:%02d", math.floor(seconds / 60), seconds % 60)
end

--- What came back from a run, as one line for the band.
local function first_line(text)
  return ((text or ""):match("[^\n]+") or ""):gsub("^%s+", ""):gsub("%s+$", "")
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
    say("voice: " .. why:gsub("^Error: ", ""), "error")
    return
  end
  if ask.verb == "start" then
    state.phase = "recording"
    state.since = nil
    return
  end
  state.phase = nil
  if ask.verb == "stop" then
    local target = state.session_name or "session"
    if out:match("^dictated") then
      say("✓ " .. out .. " → " .. target .. " (review, then Enter)", "success")
    else
      say("voice: " .. (out ~= "" and out or "nothing transcribed"))
    end
  elseif ask.verb == "cancel" then
    say("voice: recording discarded")
  end
end

return {
  name = NAME,
  -- A slot the arrangement never places: this only ever floats, and never
  -- actually returns a float, so it never takes a key.
  slot = "float",
  floats = true,
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
    if not run then
      return { type = "text", text = "" }
    end
    local now = ctx.elapsed or 0

    local ask = state.ask
    if ask then
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

    if state.phase == "recording" then
      state.since = state.since or now
      if not state.said or now - state.said >= REFRESH then
        state.said = now
        say(
          "● REC "
            .. clock(now - state.since)
            .. " → "
            .. (state.session_name or "session")
            .. " · "
            .. engine()
            .. " · ctrl+space to stop"
        )
      end
    elseif state.phase == "stopping" and (not state.said or now - state.said >= REFRESH) then
      state.said = now
      say("⋯ transcribing for " .. (state.session_name or "session"))
    end

    return { type = "text", text = "" }
  end,

  on_action = function(action)
    if action == TOGGLE then
      if not run then
        say("voice: trust this plugin first — Ctrl+, then ] then t on voice", "error")
        return true
      end
      if state.phase == nil then
        local session = selected()
        if not session then
          say("voice: select a session to dictate into", "error")
          return true
        end
        state.phase = "starting"
        state.session_name = session.name or session.id
        state.said = nil
        queue(
          "start",
          BIN .. " start --session " .. quote(session.id) .. " --engine " .. quote(engine())
        )
      elseif state.phase == "starting" or state.phase == "recording" then
        state.phase = "stopping"
        state.said = nil
        queue("stop", BIN .. " stop")
      else
        say("voice: still transcribing — one moment")
      end
      return true
    end

    if action == CANCEL then
      if state.phase == "recording" or state.phase == "starting" then
        state.phase = "cancelling"
        queue("cancel", BIN .. " cancel")
      else
        say("voice: not recording")
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
      say("voice: engine → " .. nextone .. " (from the next dictation)")
      return true
    end

    return false
  end,
}
