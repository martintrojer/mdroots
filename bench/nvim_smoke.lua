-- Headless smoke test for editors/nvim ([Neovim](https://neovim.io) 0.12+) against the real
-- `mdroots lsp` server. Read-only on two vaults given by $MDROOTS_VAULT_A and
-- $MDROOTS_VAULT_B (defaults: tests/corpus/zkvault and tests/corpus/notesvault):
-- buffers there are only opened and queried. Edits (completion probe, broken
-- link, extract note, note rename) go to scratch notes under
-- /tmp/mdroots-smoke, which is wiped at the start, and the server's cache dir
-- is pointed there as well.
--
--   cargo build -p mdroots-cli
--   nvim --clean --headless -u NONE -c 'luafile bench/nvim_smoke.lua'   (from the repo root)
--
-- Binary: $MDROOTS_BIN, else $CARGO_TARGET_DIR/debug/mdroots, else
-- <repo>/target/debug/mdroots. Exit code 0 = all assertions passed, 1 = at
-- least one failed (or no binary).

local repo = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p:h:h')
vim.opt.rtp:prepend(repo .. '/editors/nvim')
local vault_a = os.getenv('MDROOTS_VAULT_A') or (repo .. '/tests/corpus/zkvault')
local vault_b = os.getenv('MDROOTS_VAULT_B') or (repo .. '/tests/corpus/notesvault')
local dir = '/tmp/mdroots-smoke'
vim.fn.delete(dir, 'rf')
vim.fn.mkdir(dir, 'p')
local target = os.getenv('CARGO_TARGET_DIR')
local bin = os.getenv('MDROOTS_BIN')
  or (target and target ~= '' and (target .. '/debug/mdroots'))
  or (repo .. '/target/debug/mdroots')
if vim.fn.executable(bin) ~= 1 then
  io.stdout:write('FAIL no mdroots binary at ' .. bin .. ' (run: cargo build -p mdroots-cli, or set MDROOTS_BIN)\n')
  vim.cmd('cq! 1')
end
vim.lsp.config('mdroots', { cmd = { bin, 'lsp' }, cmd_env = { XDG_CACHE_HOME = dir .. '/cache' } })
vim.cmd('runtime! plugin/mdroots.lua')

local out, failed = {}, 0
local function say(s) table.insert(out, s) end
local function check(name, ok, detail)
  if not ok then failed = failed + 1 end
  say(string.format('%-4s %-30s %s', ok and 'ok' or 'FAIL', name, detail or ''))
end
local function finish()
  say(failed == 0 and 'PASS' or ('FAILED: ' .. failed))
  io.stdout:write(table.concat(out, '\n') .. '\n')
  vim.cmd(failed == 0 and 'qa!' or 'cq! 1')
end
local function ms(t0) return math.floor((vim.uv.hrtime() - t0) / 1e6) end
local function client_of(buf) return vim.lsp.get_clients({ bufnr = buf, name = 'mdroots' })[1] end
local function maps_of(buf)
  local m = {}
  for _, k in ipairs(vim.api.nvim_buf_get_keymap(buf, 'n')) do
    if (k.desc or ''):match('^mdroots') then m[k.lhs] = k end
  end
  for _, k in ipairs(vim.api.nvim_buf_get_keymap(buf, 'x')) do
    if (k.desc or ''):match('^mdroots') then m['x:' .. k.lhs] = k end
  end
  return m
end
local function loclist_len(win) return #vim.fn.getloclist(win) end

-- Scratch notes: one with two headings and three in-document links, and one
-- linking to it (backlinks, rename).
local loose = dir .. '/loose-note.md'
vim.fn.writefile({
  '# Loose note', '', 'See [the setup](#setup) and [[#Usage]].', '',
  '## Setup', '', 'Text.', '', '## Usage', '', 'Back to [setup](#setup).',
}, loose)
local linker = dir .. '/linker.md'
vim.fn.writefile({ '# Linker', '', 'Points at [[loose-note]].' }, linker)

-- 1. Attach: four buffers in two marker roots and one loose dir, one client.
local t0 = vim.uv.hrtime()
local bufs = {}
for _, p in ipairs({ vault_a .. '/README.md', vault_b .. '/README.md', linker, loose }) do
  vim.cmd('edit ' .. p)
  local b = vim.api.nvim_get_current_buf()
  local ok = vim.wait(5000, function() local c = client_of(b); return c ~= nil and c.initialized end, 20)
  check('attach ' .. vim.fn.fnamemodify(p, ':t'), ok, ms(t0) .. ' ms')
  table.insert(bufs, b)
end
local b_a, b_b, b_l, b = bufs[1], bufs[2], bufs[3], bufs[4]
local clients = vim.lsp.get_clients({ name = 'mdroots' })
local c = clients[1]
if not c then return finish() end
check('one client for all buffers', #clients == 1, 'clients=' .. #clients)
check('root_dir nil', c.root_dir == nil, 'root_dir=' .. tostring(c.root_dir))
check('settings block sent', type(c.settings.mdroots) == 'table', 'settings.mdroots=' .. type(c.settings.mdroots))
check('position encoding utf-8', c.offset_encoding == 'utf-8', c.offset_encoding)

-- 2. Buffer-local setup from plugin/mdroots.lua.
local m = maps_of(b_b)
local names = vim.tbl_keys(m); table.sort(names)
check('maps installed', m['gd'] and m['gO'] and m['\\nb'] and m['\\ns'] and m['\\nr'] and m['x:\\nn'] and true or false,
  table.concat(names, ','))
check('gO overrides ftplugin', (vim.fn.maparg('gO', 'n', false, true).desc or '') == 'mdroots: document symbols')
check(':MdrootsInfo', vim.api.nvim_buf_get_commands(b_b, {}).MdrootsInfo ~= nil)
check('omnifunc', vim.bo[b].omnifunc == 'v:lua.vim.lsp.omnifunc', vim.bo[b].omnifunc)

-- 3. Wait for the server to finish indexing the loose note: poll goto on
--    [the setup](#setup) until it resolves (not a fixed sleep).
vim.api.nvim_set_current_buf(b)
local uri = vim.uri_from_bufnr(b)
t0 = vim.uv.hrtime()
local def
vim.wait(15000, function()
  local r = c:request_sync('textDocument/definition', { textDocument = { uri = uri }, position = { line = 2, character = 8 } }, 1000, b)
  def = r and r.result
  if def and def.uri == nil then def = def[1] end
  return def ~= nil
end, 100)
check('definition #setup -> line 5', def ~= nil and def.range.start.line == 4,
  def and ('line ' .. (def.range.start.line + 1) .. ' after ' .. ms(t0) .. ' ms') or 'unresolved')

-- 4. Document symbols on both vaults and the loose note.
for _, x in ipairs({ { 'vault A README', b_a }, { 'vault B README', b_b }, { 'loose-note.md', b } }) do
  local r = c:request_sync('textDocument/documentSymbol', { textDocument = { uri = vim.uri_from_bufnr(x[2]) } }, 3000, x[2])
  local n = 0
  local function count(list) for _, s in ipairs(list or {}) do n = n + 1; count(s.children) end end
  count(r and r.result)
  check('symbols ' .. x[1], n > 0, n .. ' headings')
end

-- 5. gO (LSP symbols into the loclist) on the loose note.
local win = vim.api.nvim_get_current_win()
vim.fn.setloclist(win, {}, 'r', { items = {} })
vim.fn.maparg('gO', 'n', false, true).callback()
vim.wait(3000, function() return loclist_len(win) > 0 end, 20)
check('gO -> loclist', loclist_len(win) == 3, loclist_len(win) .. ' entries')
vim.cmd('lclose')

-- 6. Backlinks through exec_cmd and the plugin's loclist handler: the note's
--    own anchor links are excluded, so only linker.md line 3 remains.
vim.api.nvim_win_set_cursor(win, { 5, 3 }) -- on "## Setup"
vim.fn.setloclist(win, {}, 'r', { items = {} })
maps_of(b)['\\nb'].callback()
vim.wait(3000, function() return loclist_len(win) > 0 end, 20)
local ll = vim.fn.getloclist(win)
local ll_name = ll[1] and vim.api.nvim_buf_get_name(ll[1].bufnr) or ''
check('backlinks -> loclist', #ll == 1 and ll_name:match('/linker%.md$') ~= nil and ll[1].lnum == 3,
  #ll .. ' entries' .. (#ll > 0 and (', ' .. vim.fn.fnamemodify(ll_name, ':t') .. ':' .. ll[1].lnum) or ''))
vim.cmd('lclose')

-- 7. Completion after [[# (heading completion in the same document).
vim.api.nvim_buf_set_lines(b, -1, -1, false, { '[[#' })
local last = vim.api.nvim_buf_line_count(b) - 1
local items
vim.wait(3000, function()
  local r = c:request_sync('textDocument/completion', { textDocument = { uri = uri }, position = { line = last, character = 3 } }, 1000, b)
  items = r and r.result and (r.result.items or r.result)
  return items and #items > 0
end, 100)
local labels = vim.tbl_map(function(i) return i.label end, items or {})
table.sort(labels)
check('completion [[#', table.concat(labels, ',') == 'Setup,Usage', table.concat(labels, ','))
vim.api.nvim_buf_set_lines(b, last, last + 1, false, {})

-- 8. `#` at line start is a heading, not a tag: no completion items.
vim.api.nvim_buf_set_lines(b, -1, -1, false, { '#' })
last = vim.api.nvim_buf_line_count(b) - 1
local r = c:request_sync('textDocument/completion', { textDocument = { uri = uri }, position = { line = last, character = 1 } }, 2000, b)
local hitems = r and r.result and (r.result.items or r.result) or {}
check('no completion for line-start #', r ~= nil and r.err == nil and #hitems == 0, #hitems .. ' items')
vim.api.nvim_buf_set_lines(b, last, last + 1, false, {})

-- 9. Diagnostics for a broken link in the scratch note.
vim.api.nvim_buf_set_lines(b, -1, -1, false, { 'Gone: [[no-such-note-anywhere]].' })
last = vim.api.nvim_buf_line_count(b) - 1
local diag
vim.wait(5000, function()
  for _, d in ipairs(vim.diagnostic.get(b)) do
    if d.lnum == last then diag = d; return true end
  end
  return false
end, 50)
check('diagnostic for broken link', diag ~= nil, diag and diag.message or 'none')
vim.api.nvim_buf_set_lines(b, last, last + 1, false, {})

-- 10. Cross-file goto from vault B's README (its first [[link]]).
local lnum, col
for i, line in ipairs(vim.api.nvim_buf_get_lines(b_b, 0, -1, false)) do
  local s = line:find('%[%[')
  if s then lnum, col = i - 1, s + 1; break end
end
local res
if lnum then
  local rr = c:request_sync('textDocument/definition',
    { textDocument = { uri = vim.uri_from_bufnr(b_b) }, position = { line = lnum, character = col } }, 2000, b_b)
  res = rr and rr.result
  res = res and (res.uri or (res[1] and res[1].uri))
end
local res_path = res and vim.uri_to_fname(res)
check('cross-file goto vault B', res_path ~= nil and vim.fn.filereadable(res_path) == 1
  and vim.startswith(res_path, vim.fn.fnamemodify(vault_b, ':p')),
  res_path and vim.fn.fnamemodify(res_path, ':.') or 'unresolved')

-- 11. :MdrootsInfo reports vault B's root via window/showMessage.
local msgs = {}
local show = vim.lsp.handlers['window/showMessage']
vim.lsp.handlers['window/showMessage'] = function(err, res2, ctx)
  table.insert(msgs, res2 and res2.message or '')
end
local notify = vim.notify
vim.notify = function(m2) table.insert(msgs, tostring(m2)) end
vim.api.nvim_set_current_buf(b_b)
vim.cmd('MdrootsInfo')
local root_b = vim.fn.fnamemodify(vault_b, ':p'):gsub('/$', '')
local info_ok = vim.wait(3000, function()
  for _, m2 in ipairs(msgs) do
    if m2:find('root: ', 1, true) and m2:find(root_b, 1, true) then return true end
  end
  return false
end, 20)
vim.lsp.handlers['window/showMessage'] = show
vim.notify = notify
check(':MdrootsInfo shows root', info_ok, (msgs[1] or 'no message'):gsub('\n.*', ''))

-- 12. Folding and code lenses: the loose note folds through the LSP, and
--     vault A's gardening note gets a "6 backlinks" lens (from the README,
--     composting, a journal week, both meeting notes and an org file).
local fold_win = vim.api.nvim_get_current_win()
local prev_buf = vim.api.nvim_win_get_buf(fold_win)
vim.api.nvim_win_set_buf(fold_win, b)
check('foldexpr set', vim.wo[fold_win].foldexpr == 'v:lua.vim.lsp.foldexpr()', vim.wo[fold_win].foldexpr)
vim.cmd('edit ' .. vault_a .. '/reference/gardening.md')
local g = vim.api.nvim_get_current_buf()
vim.wait(5000, function() local cl = client_of(g); return cl ~= nil and cl.initialized end, 20)
local lens_title
vim.wait(5000, function()
  for _, item in ipairs(vim.lsp.codelens.get({ bufnr = g })) do
    local t = item.lens.command and item.lens.command.title or ''
    if t:match('^%d+ backlinks?$') then lens_title = t; return true end
  end
  return false
end, 50)
check('codelens backlinks', lens_title == '6 backlinks', lens_title or 'none')
vim.api.nvim_win_set_buf(fold_win, prev_buf)
vim.api.nvim_buf_delete(g, {})

-- 13. Extract note: select the loose note's first two lines (linewise, so
--     the range ends at the last character of line 2) and run the
--     <leader>nn map. The client applies CreateFile (an empty file on disk;
--     the content stays in its unsaved buffer) and the link edit. The H1
--     "Loose note" names it; loose-note.md exists, so it is loose-note-2.md.
vim.api.nvim_set_current_buf(b)
local extracted = dir .. '/loose-note-2.md'
local notify0 = vim.notify
vim.notify = function() end
vim.cmd('normal! ggVj')
local visual = vim.api.nvim_get_mode().mode
maps_of(b)['x:\\nn'].callback()
local created = vim.wait(5000, function() return vim.fn.filereadable(extracted) == 1 end, 20)
vim.cmd('normal! \27')
vim.notify = notify0
check('extract in visual mode', visual == 'V', 'mode ' .. visual)
check('extract created file', created, created and 'loose-note-2.md' or 'no file')
local nb = vim.fn.bufnr(extracted)
local first = nb > 0 and (vim.api.nvim_buf_get_lines(nb, 0, 1, false)[1] or '') or ''
check('extract buffer has the text', vim.startswith(first, '# '), first)
local stext = table.concat(vim.api.nvim_buf_get_lines(b, 0, -1, false), '\n')
local slink = stext:match('%[%[[^%]]*loose%-note%-2[^%]]*%]%]') or stext:match('%[[^%]]*%]%([^)]*loose%-note%-2[^)]*%)')
check('extract linked the selection', slink ~= nil and not stext:find('# Loose note', 1, true), slink or stext:sub(1, 40))
local estrays = vim.fn.glob(vim.fn.fnamemodify(vault_a, ':p') .. '**/loose-note-2.md', true, true)
vim.list_extend(estrays, vim.fn.glob(vim.fn.fnamemodify(vault_b, ':p') .. '**/loose-note-2.md', true, true))
check('extract wrote only in scratch', #estrays == 0 and vim.startswith(vim.uv.fs_realpath(extracted) or '', vim.uv.fs_realpath(dir)),
  #estrays .. ' strays')
check('extract left vaults unmodified', not vim.bo[b_a].modified and not vim.bo[b_b].modified)

-- 14. Rename (last): mdroots.renameFile on the scratch note. The server sends
--     workspace/applyEdit; the client fixes linker.md and moves the file.
vim.api.nvim_set_current_buf(b)
local renamed = dir .. '/renamed-note.md'
local applied
local apply = vim.lsp.handlers['workspace/applyEdit']
vim.lsp.handlers['workspace/applyEdit'] = function(err, params, ctx)
  -- Quiet the handler's print and the :saveas message.
  local print0 = print
  _G.print = function() end
  _G.mdroots_smoke_apply = function() applied = apply(err, params, ctx) end
  vim.cmd('silent lua mdroots_smoke_apply()')
  _G.print, _G.mdroots_smoke_apply = print0, nil
  return applied
end
c:exec_cmd({
  command = 'mdroots.renameFile',
  arguments = { vim.uri_from_fname(loose), vim.uri_from_fname(renamed) },
}, { bufnr = b })
local moved = vim.wait(5000, function()
  return vim.fn.filereadable(renamed) == 1 and vim.fn.filereadable(loose) == 0
end, 20)
vim.lsp.handlers['workspace/applyEdit'] = apply
check('workspace/applyEdit applied', applied ~= nil and applied.applied == true,
  applied and tostring(applied.failureReason or 'applied') or 'not received')
local ltext = table.concat(vim.api.nvim_buf_get_lines(b_l, 0, -1, false), '\n')
check('rename moved file', moved, moved and 'renamed-note.md' or 'old file still there')
check('rename fixed link', ltext:find('[[renamed-note]]', 1, true) ~= nil, (ltext:match('Points at [^\n]*') or ''))
local cur = vim.api.nvim_buf_get_name(vim.api.nvim_get_current_buf())
check('rename buffer renamed', vim.uv.fs_realpath(cur) == vim.uv.fs_realpath(renamed), vim.fn.fnamemodify(cur, ':t'))
local strays = vim.fn.glob(vim.fn.fnamemodify(vault_a, ':p') .. '**/renamed-note.md', true, true)
vim.list_extend(strays, vim.fn.glob(vim.fn.fnamemodify(vault_b, ':p') .. '**/renamed-note.md', true, true))
check('nothing written in vaults', #strays == 0, #strays .. ' strays')

-- Read-only guard: no vault buffer was modified.
check('vault buffers unmodified', not vim.bo[b_a].modified and not vim.bo[b_b].modified)
finish()
