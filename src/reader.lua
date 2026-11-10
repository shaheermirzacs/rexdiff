local channel = ...
local api = vim.api
local ns = api.nvim_create_namespace('rexdiff')
local notes_ns = api.nvim_create_namespace('rexdiff-notes')
local buffer = api.nvim_get_current_buf()
local rows, generation, path = {}, 0, nil
local number_width = 2
vim.o.termguicolors = true
vim.o.number = true
vim.o.signcolumn = 'no'
vim.o.foldcolumn = '0'
vim.o.wrap = true
vim.o.linebreak = true
vim.o.breakindent = true
vim.o.laststatus = 0
vim.o.showmode = false
vim.o.showcmd = false
vim.o.ruler = false
vim.o.mouse = 'a'
vim.o.ignorecase = true
vim.o.smartcase = true
vim.o.hlsearch = true
vim.o.incsearch = true
vim.o.swapfile = false
vim.o.modeline = false
vim.o.undofile = false
vim.o.fillchars = 'eob: '
vim.o.shortmess = vim.o.shortmess .. 'I'
vim.bo.buftype = 'nofile'
vim.bo.bufhidden = 'hide'
vim.bo.modifiable = false

local function emit(action, argument)
  if api.nvim_get_current_buf() ~= buffer then
    vim.notify('Return to the rexdiff buffer before using review actions',vim.log.levels.WARN)
    return
  end
  local cursor = api.nvim_win_get_cursor(0)
  local mode = vim.fn.mode()
  local capture = {}
  if mode == 'v' or mode == 'V' or mode == '\22' then
    local anchor, finish = vim.fn.getpos('v'), vim.fn.getpos('.')
    local first = math.min(anchor[2], finish[2])
    local text = vim.fn.getregion(anchor, finish, {type=mode, exclusive=vim.o.selection=='exclusive'})
    for i, line in ipairs(text) do capture[i] = {row=first+i-2, text=line} end
  end
  vim.rpcnotify(channel, 'rexdiff', {action=action, argument=argument or '',
    generation=generation, row=cursor[1]-1, byte=cursor[2], selection=capture})
  if mode == 'v' or mode == 'V' or mode == '\22' then
    api.nvim_feedkeys(api.nvim_replace_termcodes('<Esc>',true,false,true),'n',false)
  end
end
for key, action in pairs({['<Space>a']='annotate', ['<Space>s']='send',
  ['<Space>l']='notes', ['<Space>x']='context', ['<Space>p']='picker',
  ['<Tab>']='focus', ['<S-Tab>']='focus', ['\\']='sidebar'}) do
  vim.keymap.set({'n','x'}, key, function() emit(action) end)
end
api.nvim_create_user_command('Open', function(args) emit('open',args.args) end, {nargs=1})
api.nvim_create_user_command('Source', function(args) emit('source',args.args) end, {nargs=1})
api.nvim_create_user_command('ReviewQuit', function() emit('quit') end, {bang=true})
-- Keep :e inside the project's path validation and :q inside app cleanup.
for command, replacement in pairs({e='Open', edit='Open', q='ReviewQuit', quit='ReviewQuit'}) do
  vim.keymap.set('ca', command, function()
    return vim.fn.getcmdtype()==':' and vim.fn.getcmdline()==command and replacement or command
  end, {expr=true})
end

function _G.RexDiffGutter()
  local row = rows[vim.v.lnum]
  if not row then return ' ' end
  local group = 'Rx'..row.kind..'Gutter'
  local mark = row.note and '%#RxAnnotation#▌' or '%#'..group..'# '
  if vim.v.virtnum < 0 then return string.rep(' ',number_width+4) end
  if vim.v.virtnum > 0 then return mark..'%#'..group..'#'..string.rep(' ',number_width+2)..'%* ' end
  return mark..'%#'..group..'#'..string.format('%'..number_width..'s ',row.number)..row.sign..'%* '
end
vim.o.statuscolumn = '%!v:lua.RexDiffGutter()'

function _G.RexDiffLoad(document)
  if api.nvim_get_current_buf() ~= buffer then api.nvim_set_current_buf(buffer) end
  local cursor = api.nvim_win_get_cursor(0)
  local previous = rows[cursor[1]]
  local changed = path ~= document.path or #rows ~= #document.rows
  if not changed then
    for i,row in ipairs(rows) do
      if row.text ~= document.rows[i].text or row.old ~= document.rows[i].old or row.new ~= document.rows[i].new then changed=true; break end
    end
  end
  if document.seek or path ~= document.path then
    cursor = {document.cursor[1]+1,document.cursor[2]}
  elseif changed and previous then
    for i,row in ipairs(document.rows) do
      if (previous.new ~= vim.NIL and previous.new == row.new) or
        (previous.new == vim.NIL and previous.old ~= vim.NIL and previous.old == row.old) then cursor[1]=i; break end
    end
  end
  rows, generation, path = document.rows, document.generation, document.path
  api.nvim_buf_clear_namespace(buffer,notes_ns,0,-1)
  number_width = math.max(2,tostring(document.max_lineno):len())
  for name, attributes in pairs(document.highlights) do api.nvim_set_hl(0,name,attributes) end
  local lines = {}
  for i,row in ipairs(rows) do lines[i] = row.text end
  if changed then
    vim.cmd('stopinsert')
    api.nvim_feedkeys(api.nvim_replace_termcodes('<Esc>',true,false,true),'nx',false)
    vim.bo.modifiable = true
    api.nvim_buf_set_lines(0,0,-1,false,lines)
    vim.bo.modifiable = false
    vim.bo.modified = false
  end
  api.nvim_buf_clear_namespace(0,ns,0,-1)
  for i,row in ipairs(rows) do
    api.nvim_buf_set_extmark(0,ns,i-1,0,{end_row=i,end_col=0,
      hl_group='Rx'..row.kind,hl_eol=true,priority=50})
    for _,span in ipairs(row.spans) do
      api.nvim_buf_set_extmark(0,ns,i-1,span.start,{end_col=span.finish,
        hl_group=span.group,priority=100})
    end
  end
  if changed or document.seek then
    api.nvim_win_set_cursor(0,{math.max(1,math.min(cursor[1],#lines)),cursor[2]})
  end
  vim.cmd('redraw!')
end

function _G.RexDiffNotes(notes)
  api.nvim_buf_clear_namespace(buffer,notes_ns,0,-1)
  for _,row in ipairs(rows) do row.note=false end
  for _,note in ipairs(notes) do
    for _,line in ipairs(note.rows) do if rows[line+1] then rows[line+1].note=true end end
    local first = note.rows[1]
    if first and rows[first+1] then
      local callout = {}
      for i,line in ipairs(vim.split(note.comment,'\n',{plain=true})) do
        callout[i] = {{(i==1 and '╭─ 💬 ' or '│  ')..line,'RxAnnotation'}}
      end
      api.nvim_buf_set_extmark(buffer,notes_ns,first,0,{virt_lines=callout,virt_lines_above=true})
    end
  end
  vim.cmd('redraw!')
end
