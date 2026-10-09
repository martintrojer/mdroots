-- ~/.config/nvim/lsp/mdroots.lua          (Neovim 0.12+, https://neovim.io)
--
-- The whole config. mdroots finds roots by itself, so no root_markers or
-- root_dir are set here: Neovim starts the server even outside any project
-- (root_dir stays nil), and a single process serves every root, nested roots
-- included. Enable it from init.lua with:  vim.lsp.enable('mdroots')
--
-- Filetypes: markdown and org. mdx, quarto and rmd are excluded for now.
-- Position encoding: Neovim offers utf-8 first and mdroots picks it, so no
-- UTF-16 conversion happens on either side.
-- Large or cold roots: the server answers for an opened note at once and
-- indexes its root in the background; an open over a second shows LSP
-- progress ("mdroots: indexing <dir>").

---@type vim.lsp.Config
return {
  -- The `lsp` subcommand is required. mdroots never starts an LSP server
  -- implicitly, because a bare `mdroots` under CI or cron (stdin not a TTY)
  -- would otherwise sit waiting for JSON-RPC.
  cmd = { 'mdroots', 'lsp' }, -- or { 'mdroots', 'lsp', '--log', vim.fn.stdpath('log') .. '/mdroots.log' }
  filetypes = { 'markdown', 'org' },

  -- One client for all buffers. With root_dir nil (this config), Neovim's
  -- default already reuses the client, so this predicate only matters when a
  -- plugin or your own config sets root_dir. It then keeps a single process,
  -- but the server never hears about that folder: a reused client gets no
  -- workspace/didChangeWorkspaceFolders. That is fine because mdroots
  -- discovers roots from the file path, not from workspace folders.
  reuse_client = function(client, config)
    return client.name == config.name and not client:is_stopped()
  end,

  workspace_required = false,

  -- Everything is optional, and the defaults are meant to be right. Neovim
  -- sends `settings` in workspace/didChangeConfiguration right after
  -- initialize and again whenever you change client.settings. Keys live
  -- under `mdroots`.
  settings = {
    mdroots = {
      -- diagnostics = 'auto',       -- 'auto' | 'off' | 'hint' | 'warn' | 'error'
      --                                (the severity of broken links and anchors)
    },
  },
}
