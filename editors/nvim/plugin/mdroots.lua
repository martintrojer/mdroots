-- ~/.config/nvim/plugin/mdroots.lua        (Neovim 0.12+, https://neovim.io)
--
-- Optional extras on top of `vim.lsp.enable('mdroots')`. Everything below works
-- with the built-in client; no plugins are needed. Delete what you don't want.
--
-- Built-in defaults you already get once mdroots attaches (nothing to map):
--   K      hover (note preview)        grn  on a link or the H1: rename the note
--   grr    references / backlinks           (file and links to it)
--   ]d [d  next/prev broken link       <C-]> goto via tagfunc (follows [[links]])
--                                      <C-x><C-o> completion via omnifunc
--   gra    code actions (on a visual selection: extract to a new note)
-- gO is NOT among them for markdown: the markdown ftplugin maps it to a
-- treesitter outline. The LspAttach handler below remaps it to LSP symbols.
--
-- Using nvim-cmp or blink.cmp? Set `vim.g.mdroots_autocomplete = false` before
-- this file loads, so the built-in autotrigger does not compete with them.

vim.lsp.enable('mdroots')

local group = vim.api.nvim_create_augroup('mdroots', { clear = true })

-- Result handler for mdroots.backlinks: Location[] -> location list.
-- Client:exec_cmd() discards results unless a handler is passed.
local function backlinks_to_loclist(err, result, ctx)
  if err then
    vim.notify('mdroots.backlinks: ' .. (err.message or tostring(err)), vim.log.levels.WARN)
    return
  end
  local client = vim.lsp.get_client_by_id(ctx.client_id)
  if not client then
    return
  end
  local items = vim.lsp.util.locations_to_items(result or {}, client.offset_encoding)
  local win = vim.fn.bufwinid(ctx.bufnr)
  win = win ~= -1 and win or 0
  vim.fn.setloclist(win, {}, ' ', { title = 'mdroots backlinks', items = items })
  if #items > 0 then
    vim.api.nvim_win_call(win, function()
      vim.cmd('lopen')
    end)
  else
    vim.notify('mdroots: no backlinks', vim.log.levels.INFO)
  end
end

-- The line `lnum` (1-based) of `fname` for a location without a message: from
-- the loaded buffer when there is one (it may hold unsaved edits), else disk.
local function line_text(fname, lnum)
  local b = vim.fn.bufnr(fname)
  if b ~= -1 and vim.api.nvim_buf_is_loaded(b) then
    return vim.api.nvim_buf_get_lines(b, lnum - 1, lnum, false)[1] or ''
  end
  local ok, lines = pcall(vim.fn.readfile, fname, '', lnum)
  return ok and lines[lnum] or ''
end

-- Result handler for the mdroots note-list commands (orphans, missingBacklinks,
-- related, search, ...): `{uri, range, message?}[]` -> quickfix list, one item
-- per location in server order. Not vim.lsp.util.locations_to_items: it
-- re-sorts by file and drops `message`.
local function ordered_to_qflist(title)
  return function(err, result)
    if err then
      vim.notify(title .. ': ' .. (err.message or tostring(err)), vim.log.levels.WARN)
      return
    end
    local items = {}
    for _, loc in ipairs(result or {}) do
      local fname = vim.uri_to_fname(loc.uri)
      local lnum = loc.range.start.line + 1
      table.insert(items, {
        filename = fname,
        lnum = lnum,
        col = loc.range.start.character + 1,
        text = loc.message or line_text(fname, lnum),
      })
    end
    vim.fn.setqflist({}, ' ', { title = title, items = items })
    if #items > 0 then
      vim.cmd('copen')
    else
      vim.notify(title .. ': nothing found', vim.log.levels.INFO)
    end
  end
end

-- Result handler for mdroots.check: `{uri, diagnostics}[]` -> quickfix list of
-- every diagnostic, notes by path.
local severity_type = { 'E', 'W', 'I', 'N' }
local function check_to_qflist(err, result)
  if err then
    vim.notify('mdroots.check: ' .. (err.message or tostring(err)), vim.log.levels.WARN)
    return
  end
  local items = {}
  for _, entry in ipairs(result or {}) do
    local fname = vim.uri_to_fname(entry.uri)
    for _, d in ipairs(entry.diagnostics) do
      table.insert(items, {
        filename = fname,
        lnum = d.range.start.line + 1,
        col = d.range.start.character + 1,
        type = severity_type[d.severity or 1],
        text = d.message,
      })
    end
  end
  vim.fn.setqflist({}, ' ', { title = 'mdroots check', items = items })
  if #items > 0 then
    vim.cmd('copen')
  else
    vim.notify('mdroots: no diagnostics', vim.log.levels.INFO)
  end
end

-- Called with the tag name chosen in the <leader>nt picker. Does nothing yet.
local function on_tag_selected(name) -- luacheck: ignore
end

vim.api.nvim_create_autocmd('LspAttach', {
  group = group,
  callback = function(ev)
    local client = vim.lsp.get_client_by_id(ev.data.client_id)
    if not client or client.name ~= 'mdroots' then
      return
    end
    local buf = ev.buf
    local function map(lhs, rhs, desc, mode)
      vim.keymap.set(mode or 'n', lhs, rhs, { buffer = buf, desc = 'mdroots: ' .. desc })
    end

    -- gd on any link form: [[wiki]], [md](rel/path), org [[file:..]], bare paths.
    map('gd', vim.lsp.buf.definition, 'goto link target')

    -- Headings outline from the server (replaces the ftplugin's treesitter gO).
    if client:supports_method('textDocument/documentSymbol') then
      map('gO', vim.lsp.buf.document_symbol, 'document symbols')
    end

    -- Live completion as you type [[, ]( or # (tags; not a heading's leading #).
    if vim.g.mdroots_autocomplete ~= false and client:supports_method('textDocument/completion') then
      vim.lsp.completion.enable(true, client.id, buf, { autotrigger = true })
    end

    -- "N backlinks" lens above the note's title and "N links" lenses above
    -- headings other notes link to by anchor.
    if client:supports_method('textDocument/codeLens') then
      vim.lsp.codelens.enable(true, { bufnr = buf })
    end

    -- Fold by heading sections and the frontmatter block.
    if client:supports_method('textDocument/foldingRange') then
      local win = vim.api.nvim_get_current_win()
      vim.wo[win][0].foldmethod = 'expr'
      vim.wo[win][0].foldexpr = 'v:lua.vim.lsp.foldexpr()'
      vim.wo[win][0].foldlevel = 99
    end

    -- Workspace-wide note search (fuzzy over titles, file names and aliases).
    map('<leader>ns', function()
      vim.lsp.buf.workspace_symbol(vim.fn.input('notes> '))
    end, 'search notes')

    -- Backlinks to the current note (not just the heading under the cursor),
    -- into the location list.
    map('<leader>nb', function()
      local pos = vim.lsp.util.make_position_params(0, client.offset_encoding).position
      client:exec_cmd({
        command = 'mdroots.backlinks',
        arguments = { vim.uri_from_bufnr(buf), pos },
      }, { bufnr = buf }, backlinks_to_loclist)
    end, 'backlinks of this note')

    local function run(command, extra, handler)
      local args = { vim.uri_from_bufnr(buf) }
      vim.list_extend(args, extra or {})
      client:exec_cmd({ command = command, arguments = args }, { bufnr = buf }, handler)
    end

    -- Whole-notebook lists into the quickfix list, in the server's order.
    map('<leader>no', function()
      run('mdroots.orphans', nil, ordered_to_qflist('mdroots orphans'))
    end, 'orphan notes')
    map('<leader>nm', function()
      run('mdroots.missingBacklinks', nil, ordered_to_qflist('mdroots missing backlinks'))
    end, 'missing backlinks')
    map('<leader>nR', function()
      run('mdroots.related', nil, ordered_to_qflist('mdroots related'))
    end, 'notes related to this one')
    map('<leader>n/', function()
      local query = vim.fn.input('search> ')
      if query ~= '' then
        run('mdroots.search', { query }, ordered_to_qflist('mdroots search: ' .. query))
      end
    end, 'full-text search')

    -- Tags with their note counts.
    map('<leader>nt', function()
      run('mdroots.tags', nil, function(err, result)
        if err then
          vim.notify('mdroots.tags: ' .. (err.message or tostring(err)), vim.log.levels.WARN)
          return
        end
        vim.ui.select(result or {}, {
          prompt = 'tags',
          format_item = function(t)
            return string.format('%s (%d)', t.name, t.count)
          end,
        }, function(choice)
          if choice then
            on_tag_selected(choice.name)
          end
        end)
      end)
    end, 'tags')

    vim.api.nvim_buf_create_user_command(buf, 'MdrootsCheck', function()
      run('mdroots.check', nil, check_to_qflist)
    end, { desc = 'mdroots: diagnostics of every note into the quickfix list' })
    vim.api.nvim_buf_create_user_command(buf, 'MdrootsReindex', function()
      run('mdroots.reindex')
    end, { desc = 'mdroots: rescan the notebook from disk' })

    -- Rename (move) the current note and fix every link to it. Neovim 0.12
    -- never sends workspace/willRenameFiles, so this goes through a command;
    -- the server replies with workspace/applyEdit (link edits + RenameFile).
    map('<leader>nr', function()
      local old = vim.api.nvim_buf_get_name(buf)
      local new = vim.fn.input('rename note to> ', old, 'file')
      if new ~= '' and new ~= old then
        client:exec_cmd({
          command = 'mdroots.renameFile',
          arguments = { vim.uri_from_bufnr(buf), vim.uri_from_fname(vim.fn.fnamemodify(new, ':p')) },
        }, { bufnr = buf })
      end
    end, 'rename this note')

    -- Create a note from the visual selection and replace it with a link in
    -- the root's link style (from an existing zk, https://github.com/zk-org/zk,
    -- or Obsidian, https://obsidian.md, config, else voted from the notes).
    -- The file is named from the selection's heading or first line, next to
    -- the current note. The server only returns the edit (create file, insert
    -- text, link); Neovim applies it, which creates an empty file and leaves
    -- the text in a loaded, unsaved buffer for it (`:wa` writes both notes).
    -- `gra` on a visual selection offers the same action.
    map('<leader>nn', function()
      vim.lsp.buf.code_action({
        filter = function(a)
          return a.kind == 'refactor.extract.note'
        end,
        apply = true,
      })
    end, 'new note from selection', 'x')

    -- Why did mdroots pick this root? Prints root, mode (marker, vcs, lazy, ...),
    -- the decision reason and the file count (via window/showMessage).
    vim.api.nvim_buf_create_user_command(buf, 'MdrootsInfo', function()
      client:exec_cmd({ command = 'mdroots.info', arguments = { vim.uri_from_bufnr(buf) } }, { bufnr = buf })
    end, { desc = 'mdroots: show root and index status' })
  end,
})
