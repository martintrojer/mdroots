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

    -- Live completion as you type [[, ](, # (tags; not a heading's leading #),
    -- or : in frontmatter.
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
