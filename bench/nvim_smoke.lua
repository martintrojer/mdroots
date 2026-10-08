-- Headless smoke test for editors/nvim (Neovim 0.12+), with marksman standing in
-- for the mdroots binary. Read-only on two vaults given by $MDROOTS_VAULT_A and
-- $MDROOTS_VAULT_B (defaults: tests/corpus/zkvault and tests/corpus/notesvault):
-- buffers there are only opened and queried; the edits (code action, completion
-- probe) go to a scratch note under /tmp, and nothing is ever written back.
--
--   nvim --clean --headless -u NONE -c 'luafile bench/nvim_smoke.lua'   (from the repo root)
--
-- Exit code 0 = all assertions passed, 1 = at least one failed.

local repo = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p:h:h')
vim.opt.rtp:prepend(repo .. '/editors/nvim')
local vault_a = os.getenv('MDROOTS_VAULT_A') or (repo .. '/tests/corpus/zkvault')
local vault_b = os.getenv('MDROOTS_VAULT_B') or (repo .. '/tests/corpus/notesvault')
vim.lsp.config('mdroots', { cmd = { 'marksman', 'server' } }) -- stand-in for { 'mdroots', 'lsp' }
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

-- Scratch note with two headings and three in-document links.
local dir = '/tmp/mdroots-smoke'
vim.fn.mkdir(dir, 'p')
local loose = dir .. '/loose-note.md'
vim.fn.writefile({
  '# Loose note', '', 'See [the setup](#setup) and [[#Usage]].', '',
  '## Setup', '', 'Text.', '', '## Usage', '', 'Back to [setup](#setup).',
}, loose)

-- 1. Attach: three buffers in two marker roots and one loose dir, one client.
local t0 = vim.uv.hrtime()
local bufs = {}
for _, p in ipairs({ vault_a .. '/README.md', vault_b .. '/README.md', loose }) do
  vim.cmd('edit ' .. p)
  local b = vim.api.nvim_get_current_buf()
  local ok = vim.wait(5000, function() local c = client_of(b); return c ~= nil and c.initialized end, 20)
  check('attach ' .. vim.fn.fnamemodify(p, ':t'), ok, ms(t0) .. ' ms')
  table.insert(bufs, b)
end
local b_a, b_b, b = bufs[1], bufs[2], bufs[3]
local clients = vim.lsp.get_clients({ name = 'mdroots' })
local c = clients[1]
if not c then return finish() end
check('one client for all buffers', #clients == 1, 'clients=' .. #clients)
check('root_dir nil', c.root_dir == nil, 'root_dir=' .. tostring(c.root_dir))
check('settings block sent', type(c.settings.mdroots) == 'table', 'settings.mdroots=' .. type(c.settings.mdroots))
say('info position encoding: ' .. c.offset_encoding .. ' (marksman; mdroots answers utf-8)')

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

-- 6. Backlinks through exec_cmd and the plugin's loclist handler. marksman has
--    no mdroots.backlinks, so the stand-in advertises it and answers it with
--    textDocument/references at the position the plugin passes.
c.server_capabilities.executeCommandProvider = { commands = { 'mdroots.backlinks' } }
local request = c.request
c.request = function(self, method, params, handler, bufnr)
  if method == 'workspace/executeCommand' and params.command == 'mdroots.backlinks' then
    local a = params.arguments
    return request(self, 'textDocument/references',
      { textDocument = { uri = a[1] }, position = a[2], context = { includeDeclaration = false } }, handler, bufnr)
  end
  return request(self, method, params, handler, bufnr)
end
vim.api.nvim_win_set_cursor(win, { 5, 3 }) -- on "## Setup"
vim.fn.setloclist(win, {}, 'r', { items = {} })
maps_of(b)['\\nb'].callback()
vim.wait(3000, function() return loclist_len(win) > 0 end, 20)
local ll = vim.fn.getloclist(win)
check('backlinks -> loclist', #ll == 2 and ll[1].lnum == 3 and ll[2].lnum == 11,
  #ll .. ' entries' .. (#ll > 0 and (', lines ' .. ll[1].lnum .. ',' .. (ll[2] and ll[2].lnum or '-')) or ''))
vim.cmd('lclose')
c.request = nil -- back to Client.request

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

-- 8. Code action through vim.lsp.buf.code_action{filter, apply} (the same call
--    the <leader>nn map makes). marksman offers a ToC action, not the
--    extract-note one, so filter on that. The edit lands in the /tmp buffer only.
vim.api.nvim_win_set_cursor(win, { 1, 0 })
vim.lsp.buf.code_action({
  filter = function(a) return a.title == 'Create a Table of Contents' end,
  apply = true,
})
local applied = vim.wait(3000, function()
  return vim.tbl_contains(vim.api.nvim_buf_get_lines(b, 0, -1, false), '<!--toc:start-->')
end, 20)
check('code action applied (ToC)', applied, applied and 'toc block inserted' or 'no edit')

-- 9. Cross-file goto from vault B's README (its first [[link]]). Informational:
--    marksman gets no folder (root_dir nil) and does no discovery of its own,
--    so it serves the vault in single-file mode. mdroots must resolve this.
local lnum, col
for i, line in ipairs(vim.api.nvim_buf_get_lines(b_b, 0, -1, false)) do
  local s = line:find('%[%[')
  if s then lnum, col = i - 1, s + 1; break end
end
local res
if lnum then
  local r = c:request_sync('textDocument/definition',
    { textDocument = { uri = vim.uri_from_bufnr(b_b) }, position = { line = lnum, character = col } }, 2000, b_b)
  res = r and r.result
end
say('info first [[link]] in vault B README: ' .. ((res and (res.uri or (res[1] and res[1].uri))) or 'unresolved (single-file mode)'))

-- Read-only guard: no vault buffer was modified.
check('vault buffers unmodified', not vim.bo[b_a].modified and not vim.bo[b_b].modified)
finish()
