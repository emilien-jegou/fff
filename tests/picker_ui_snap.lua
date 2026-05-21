--- End-to-end snapshot tests for fff.nvim's picker UI.
local MiniTest = require('mini.test')
local fixture_lib = require('tests.snapshot.fixture')

local PLUGIN_DIR = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p:h:h')
local FORCE = vim.env.UPDATE_SNAPSHOTS == '1'

local child, fixture

local function setup(geometry, opts)
  opts = opts or {}
  fixture = fixture_lib.create()
  child = MiniTest.new_child_neovim()
  child.start({
    '--clean',
    '-n',
    '-i',
    'NONE',
    '--cmd',
    string.format('let &lines = %d', geometry.rows),
    '--cmd',
    string.format('let &columns = %d', geometry.cols),
  }, {
    connection_timeout = 15000,
  })

  child.o.lines = geometry.rows
  child.o.columns = geometry.cols

  child.lua(
    string.format(
      [[
        local plugin = %q
        vim.opt.runtimepath:prepend(plugin)
        package.path = plugin .. '/lua/?.lua;' .. plugin .. '/lua/?/init.lua;' .. package.path
        vim.cmd('cd ' .. vim.fn.fnameescape(%q))
        local winborder = %q
        if winborder ~= '' then vim.o.winborder = winborder end
        vim.g.fff = {
          prompt = '> ',
          frecency = { enabled = true, db_path = %q },
          history  = { enabled = true, db_path = %q },
          logging  = { enabled = false },
          debug    = { enabled = %s, show_scores = %s },
        }
        require('fff.core').ensure_initialized()
        require('fff.rust').wait_for_initial_scan(8000)
      ]],
      PLUGIN_DIR,
      fixture.root,
      geometry.winborder or '',
      fixture.frecency_db,
      fixture.history_db,
      tostring(opts.debug == true),
      tostring(opts.debug == true)
    )
  )
end

local function teardown()
  if child and child.is_running() then pcall(child.stop) end
  if fixture then fixture_lib.cleanup(fixture) end
  child, fixture = nil, nil
end

--- @param opts table|nil { ignore_text?: number[] }
local function assert_snapshot_match(opts)
  opts = opts or {}
  MiniTest.expect.reference_screenshot(child.get_screenshot(), nil, {
    force = FORCE,
    ignore_text = opts.ignore_text or false,
  })
end

local function open_picker(prompt_position, query)
  child.lua(string.format('require("fff.picker_ui").open({ layout = { prompt_position = %q } })', prompt_position))
  vim.loop.sleep(400)

  if query and query ~= '' then
    child.type_keys(query)
    vim.loop.sleep(400)
  end
end

local LAYOUTS = {
  { name = 'wide', cols = 180, rows = 40, winborder = 'double' },
  { name = 'default', cols = 140, rows = 32 }, -- standard on most screens
  { name = 'narrow', cols = 70, rows = 24, winborder = 'rounded' },
}

local T = MiniTest.new_set()

local PROMPT_POSITIONS = { 'bottom', 'top' }

for _, geometry in ipairs(LAYOUTS) do
  local set = MiniTest.new_set({
    hooks = {
      pre_case = function() setup(geometry) end,
      post_case = teardown,
    },
  })

  -- Run every per-geometry case for both prompt positions: layout math and
  -- list rendering diverge between top/bottom (see AGENTS.md), so a snapshot
  -- on a single side would silently miss regressions in the other.
  for _, prompt in ipairs(PROMPT_POSITIONS) do
    set['empty_' .. prompt] = function()
      open_picker(prompt)
      assert_snapshot_match()
    end

    set['query_main_' .. prompt] = function()
      open_picker(prompt, 'main')
      assert_snapshot_match()
    end

    set['no_results_' .. prompt] = function()
      open_picker(prompt, 'zzzzzzzzz')
      assert_snapshot_match()
    end

    set['cursor_second_item_' .. prompt] = function()
      open_picker(prompt)
      -- Bottom prompt visually goes up with <Down>; top prompt goes down with <Down>.
      -- Either way one keypress moves to the second item — what we want to capture.
      child.type_keys('<Down>')
      vim.loop.sleep(200)
      assert_snapshot_match()
    end
  end

  T[geometry.name] = set
end

-- File info panel only renders in non-flex layouts where there's room for it,
-- so we only exercise it on the default geometry.
local default_geom = LAYOUTS[2]
local debug_set = MiniTest.new_set({
  hooks = {
    pre_case = function() setup(default_geom, { debug = true }) end,
    post_case = teardown,
  },
})

for _, prompt in ipairs(PROMPT_POSITIONS) do
  debug_set['file_info_panel_' .. prompt] = function()
    open_picker(prompt, 'main')
    -- Modified / Last Access timestamps drift between runs; ignore those text
    -- rows but still verify everything else (panel layout, scores, attrs).
    assert_snapshot_match({ ignore_text = { 13, 14 } })
  end
end
T['debug'] = debug_set

T['combo'] = MiniTest.new_set({
  hooks = {
    pre_case = function() setup({ cols = 140, rows = 32 }) end,
    post_case = teardown,
  },
})

local function train_combo()
  -- Train: "main" → src/main.rs four times so open_count crosses the default
  -- min_combo_count (3). track_query_completion is async; pause between calls
  -- + final settle for the lmdb writer.
  for _ = 1, 4 do
    child.lua(
      string.format('require("fff.rust").track_query_completion(%q, %q)', 'main', fixture.root .. '/src/main.rs')
    )
    vim.loop.sleep(120)
  end
  vim.loop.sleep(400)
end

for _, prompt in ipairs(PROMPT_POSITIONS) do
  T['combo']['boost_' .. prompt] = function()
    train_combo()
    open_picker(prompt, 'main')
    -- Combo overlay float renders asynchronously after render_list.
    vim.loop.sleep(400)
    assert_snapshot_match()
  end
end

T['scrollbar'] = MiniTest.new_set({
  hooks = {
    pre_case = function() setup({ cols = 140, rows = 32 }) end,
    post_case = teardown,
  },
})

-- Cursor advance key differs by prompt position: bottom prompt iterates the
-- list in reverse so `<Up>` walks toward higher indices; top prompt is
-- conventional. Either way we walk past the last in-page item to trigger
-- load_next_page and surface the scrollbar thumb at the new offset.
local SCROLL_KEY = { bottom = '<Up>', top = '<Down>' }

for _, prompt in ipairs(PROMPT_POSITIONS) do
  T['scrollbar']['next_page_' .. prompt] = function()
    open_picker(prompt)
    for _ = 1, 30 do
      child.type_keys(SCROLL_KEY[prompt])
      vim.loop.sleep(20)
    end
    vim.loop.sleep(400)
    assert_snapshot_match()
  end
end

return T
