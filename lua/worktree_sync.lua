-- worktree-sync for Rex: git worktree actions for Rex sessions.
--
-- Install with `worktree-sync rex lua > ~/.config/rex/worktree_sync.lua`, then
-- in init.lua:
--
--   require("worktree_sync").setup{
--     bin = "/absolute/path/to/worktree-sync",
--     keys = { open = "cmd+w", new = "shift+cmd+w", remove = "ctrl+cmd+w" },
--   }
--
-- Each action only runs `worktree-sync rex action <verb>`, which answers at
-- once: prompts and long work run in terminal blocks the binary opens, never
-- here, because Rex holds the user's keystrokes until an action returns.

local M = {}

local ACTIONS = {
  { name = "worktree_open", verb = "open", key = "open", title = "Open Worktree…",
    description = "Show the session of a worktree of this repository, creating it if needed" },
  { name = "worktree_new", verb = "new", key = "new", title = "New Worktree…",
    description = "Create a git worktree in a session of its own and set it up" },
  { name = "worktree_remove", verb = "remove", key = "remove", title = "Remove Worktree…",
    description = "Remove this session's worktree and close its sessions; the branch is kept" },
  { name = "worktree_bind", verb = "bind", key = "bind", title = "Bind Session to Checkout…",
    description = "Tell worktree-sync which checkout this session works on" },
  { name = "worktree_sync", verb = "sync", key = "sync", title = "Sync Worktree Files",
    description = "Re-apply the configured copy, clone, and symlink operations" },
}

local function is_absolute(path)
  return type(path) == "string" and path:sub(1, 1) == "/"
end

local function trim(text)
  text = text:gsub("^%s+", "")
  text = text:gsub("%s+$", "")
  return text
end

local function run_action(opts, verb, ctx)
  if not ctx.session_id then
    error("Worktree actions need a session.", 0)
  end
  if ctx.server then
    error("Worktree actions don't support remote Rex hosts yet.", 0)
  end

  local argv = { opts.bin, "rex", "action", verb, "--session", ctx.session_id }
  if ctx.block_id then
    table.insert(argv, "--block")
    table.insert(argv, ctx.block_id)
  end
  if ctx.client_id then
    table.insert(argv, "--client")
    table.insert(argv, ctx.client_id)
  end
  -- The Rex server's environment may lack REX_BIN_DIR; the binary needs it to
  -- find `rex`.
  local env = {}
  if opts.rex_bin_dir then
    env.REX_BIN_DIR = opts.rex_bin_dir
  end

  local r, err = rex.exec(argv, { timeout = 10, env = env })
  if not r then
    error("could not run " .. opts.bin .. ": " .. tostring(err), 0)
  end
  if not r.ok then
    local message = trim(r.stderr or "")
    if message == "" then
      message = r.timed_out and "worktree-sync timed out"
        or ("worktree-sync failed with exit code " .. tostring(r.code))
    end
    error(message, 0)
  end

  local out, decode_err = rex.json.decode(r.stdout)
  if type(out) ~= "table" then
    error("unexpected output from worktree-sync: " .. tostring(decode_err or r.stdout), 0)
  end
  if out.select then
    -- The work already happened; a switch that can't be queued (no client,
    -- as under `rex do`) is reported, not raised.
    local ok, queue_err = pcall(rex.client.queue, "session.select", { session_id = out.select })
    if not ok then
      return { session_id = out.select, warning = "could not switch to " .. out.select .. ": " .. tostring(queue_err) }
    end
  end
  return out
end

--- Define the worktree actions and bind the configured keys.
-- opts.bin          absolute path of the worktree-sync binary (required)
-- opts.rex_bin_dir  absolute dir of the `rex` binary (default $REX_BIN_DIR)
-- opts.keys         { open, new, remove, bind, sync } = key, each optional
function M.setup(opts)
  opts = opts or {}
  if not is_absolute(opts.bin) then
    error("worktree_sync.setup: `bin` must be the absolute path of worktree-sync", 2)
  end
  local config = { bin = opts.bin, rex_bin_dir = opts.rex_bin_dir or os.getenv("REX_BIN_DIR") }
  if config.rex_bin_dir ~= nil and not is_absolute(config.rex_bin_dir) then
    error("worktree_sync.setup: `rex_bin_dir` must be an absolute path", 2)
  end

  local keys = opts.keys or {}
  for _, action in ipairs(ACTIONS) do
    rex.action{
      name = action.name,
      title = action.title,
      description = action.description,
      category = "Worktrees",
      run = function(ctx)
        return run_action(config, action.verb, ctx)
      end,
    }
    if keys[action.key] then
      rex.bind(keys[action.key], action.name)
    end
  end
end

return M
