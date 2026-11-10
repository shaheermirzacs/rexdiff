local channel = ...
local api = vim.api
local ns = api.nvim_create_namespace('rexdiff')
local notes_ns = api.nvim_create_namespace('rexdiff-notes')
local buffer = api.nvim_get_current_buf()
local views, generation, path = {}, 0, nil
local old_buffer, old_window
local main_window = api.nvim_get_current_win()
local split = false
local note_data = {}
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
vim.o.fillchars = 'eob: ,diff: ,vert:│'
vim.o.shortmess = vim.o.shortmess .. 'I'
vim.bo.buftype = 'nofile'
vim.bo.bufhidden = 'hide'
vim.bo.modifiable = false

local function emit(action, argument)
  local rows = views[api.nvim_get_current_buf()]
  if not rows and (action == 'annotate' or action == 'source' or action == 'context') then
    vim.notify('Return to the rexdiff buffer before using review actions',vim.log.levels.WARN)
    return
  end
  rows = rows or {}
  local cursor = api.nvim_win_get_cursor(0)
  local mode = vim.fn.mode()
  local capture = {}
  if mode == 'v' or mode == 'V' or mode == '\22' then
    local anchor, finish = vim.fn.getpos('v'), vim.fn.getpos('.')
    local first = math.min(anchor[2], finish[2])
    local text = vim.fn.getregion(anchor, finish, {type=mode, exclusive=vim.o.selection=='exclusive'})
    for i, line in ipairs(text) do
      local row = rows[first+i-1]
      if row then capture[#capture+1] = {row=row.source, text=line} end
    end
  end
  vim.rpcnotify(channel, 'rexdiff', {action=action, argument=argument or '',
    generation=generation, row=rows[cursor[1]] and rows[cursor[1]].source or 0, byte=cursor[2],
    old_side=split and api.nvim_get_current_buf()==old_buffer, selection=capture})
  if mode == 'v' or mode == 'V' or mode == '\22' then
    api.nvim_feedkeys(api.nvim_replace_termcodes('<Esc>',true,false,true),'n',false)
  end
end
for key, action in pairs({['<Space>a']='annotate', ['<Space>s']='send',
  ['<Space>l']='notes', ['<Space>x']='context', ['<Space>p']='picker', ['<Space>d']='layout',
  ['<Tab>']='focus', ['<S-Tab>']='focus', ['\\']='sidebar'}) do
  vim.keymap.set({'n','x'}, key, function() emit(action) end)
end
api.nvim_create_user_command('Open', function(args) emit('open',args.args) end, {nargs=1})
local function source_line(number)
  local rows = views[api.nvim_get_current_buf()] or {}
  local fallback
  for i,row in ipairs(rows) do
    if tonumber(row.number) == number then
      fallback = fallback or i
      if split or row.kind ~= 'Del' then fallback = i; break end
    end
  end
  if fallback then
    api.nvim_win_set_cursor(0,{fallback,0})
    vim.cmd('normal! zz')
  else emit('source',tostring(number)) end
end
api.nvim_create_user_command('Source', function(args)
  local number = tonumber(args.args)
  if number then source_line(number) end
end, {nargs=1})
vim.keymap.set('c','<CR>',function()
  local line = vim.fn.getcmdline():match('^%s*(%d+)%s*$')
  if vim.fn.getcmdtype() ~= ':' then return '<CR>' end
  return line and '<C-u>Source '..line..'<CR>' or '<C-]><CR>'
end,{expr=true})
vim.keymap.set('n','G',function()
  if vim.v.count > 0 then source_line(vim.v.count) else vim.cmd('normal! G') end
end)
api.nvim_create_user_command('ReviewQuit', function() emit('quit') end, {bang=true})
-- Keep :e inside the project's path validation and :q inside app cleanup.
for command, replacement in pairs({e='Open', edit='Open', q='ReviewQuit', quit='ReviewQuit'}) do
  vim.keymap.set('ca', command, function()
    return vim.fn.getcmdtype()==':' and vim.fn.getcmdline()==command and replacement or command
  end, {expr=true})
end

function _G.RexDiffGutter()
  local rows = views[api.nvim_win_get_buf(vim.g.statusline_winid or api.nvim_get_current_win())] or {}
  local row = rows[vim.v.lnum]
  if not row then return ' ' end
  local group = 'Rx'..row.kind..'Gutter'
  if vim.v.virtnum < 0 then return string.rep(' ',number_width+4) end
  local number = row.note and '%#RxNoteJoin#' or '%#'..group..'#'
  if vim.v.virtnum > 0 then return number..string.rep(' ',number_width+2)..'%#'..group..'# %* ' end
  return number..' '..string.format('%'..number_width..'s ',row.number)..'%#'..group..'#'..row.sign..'%* '
end
vim.o.statuscolumn = '%!v:lua.RexDiffGutter()'

local function equalize()
  if split and old_window and api.nvim_win_is_valid(old_window) and api.nvim_win_is_valid(main_window) then
    api.nvim_win_call(main_window,function() vim.cmd('wincmd =') end)
  end
end
api.nvim_create_autocmd('VimResized',{callback=function()
  equalize()
  if _G.RexDiffNotes then RexDiffNotes(note_data) end
end})

local function render(document, target, next_rows, force)
  if api.nvim_get_current_buf() ~= target then api.nvim_set_current_buf(target) end
  local rows = views[target] or {}
  local cursor = api.nvim_win_get_cursor(0)
  local previous = rows[cursor[1]]
  local changed = force or path ~= document.path or #rows ~= #next_rows
  if not changed then
    for i,row in ipairs(rows) do
      if row.text ~= next_rows[i].text or row.old ~= next_rows[i].old or row.new ~= next_rows[i].new then changed=true; break end
    end
  end
  if document.seek or path ~= document.path then
    cursor = {document.cursor[1]+1,document.cursor[2]}
  elseif changed and previous then
    for i,row in ipairs(next_rows) do
      if (previous.new ~= vim.NIL and previous.new == row.new) or
        (previous.new == vim.NIL and previous.old ~= vim.NIL and previous.old == row.old) then cursor[1]=i; break end
    end
  end
  rows = next_rows
  views[target] = rows
  api.nvim_buf_clear_namespace(target,notes_ns,0,-1)
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
    local line = math.max(1,math.min(cursor[1],#lines))
    api.nvim_win_set_cursor(0,{line,math.min(cursor[2],#(lines[line] or ''))})
  end
end

function _G.RexDiffGoto(source, byte, center, old_side)
  local windows = {api.nvim_get_current_win(),main_window,old_window}
  if split and old_side ~= nil and old_side ~= vim.NIL then table.insert(windows,1,old_side and old_window or main_window) end
  for _,win in ipairs(windows) do
    if api.nvim_win_is_valid(win) then
      for i,row in ipairs(views[api.nvim_win_get_buf(win)] or {}) do
        if row.source == source then
          api.nvim_set_current_win(win)
          api.nvim_win_set_cursor(win,{i,math.min(byte,#row.text)})
          if center then vim.cmd('normal! zz') end
          return
        end
      end
    end
  end
end

function _G.RexDiffLoad(document)
  note_data = {}
  local current = api.nvim_get_current_win()
  local cursor = api.nvim_win_get_cursor(0)
  local previous = (views[api.nvim_get_current_buf()] or {})[cursor[1]]
  local changed_layout = split ~= document.split
  local changed_file = path ~= document.path
  local source = previous and previous.source or document.cursor[1]
  if path ~= document.path or document.seek then source, cursor = document.cursor[1], {1,document.cursor[2]} end
  split = document.split
  if split and (not old_window or not api.nvim_win_is_valid(old_window)) then
    api.nvim_set_current_win(main_window)
    vim.cmd('leftabove vsplit')
    old_window = api.nvim_get_current_win()
    if not old_buffer then
      old_buffer = api.nvim_create_buf(false,true)
      vim.bo[old_buffer].bufhidden = 'hide'
      vim.bo[old_buffer].modifiable = false
    end
    api.nvim_win_set_buf(old_window,old_buffer)
  elseif not split and old_window and api.nvim_win_is_valid(old_window) then
    api.nvim_win_close(old_window,true)
    old_window = nil
    api.nvim_set_current_win(main_window)
    vim.cmd('diffoff')
    vim.wo.winbar = ''
    vim.wo.wrap = true
  end
  local left, right = {}, {}
  for i,row in ipairs(document.rows) do
    row.source = i-1
    if not split or row.kind ~= 'Del' then right[#right+1] = row end
    if split and row.kind ~= 'Add' then
      local old = vim.deepcopy(row)
      if old.old ~= vim.NIL then old.number = tostring(old.old) end
      left[#left+1] = old
    end
  end
  api.nvim_win_call(main_window,function() render(document,buffer,right,changed_layout) end)
  if split then
    api.nvim_win_call(old_window,function() render(document,old_buffer,left,changed_layout) end)
    for _,win in ipairs({old_window,main_window}) do
      api.nvim_win_call(win,function()
        vim.cmd('diffthis')
        vim.wo.foldenable = false
        vim.wo.wrap = false
        vim.wo.winbar = win == old_window and ' Old' or ' New'
      end)
    end
    vim.cmd('diffupdate')
    if changed_layout then equalize() end
  end
  generation, path = document.generation, document.path
  if api.nvim_win_is_valid(current) then api.nvim_set_current_win(current) else api.nvim_set_current_win(main_window) end
  if changed_layout or changed_file or document.seek then RexDiffGoto(source,cursor[2],true) end
  vim.cmd('redraw!')
end

local function notes_for_buffer(target,rows,notes)
  api.nvim_buf_clear_namespace(target,notes_ns,0,-1)
  for _,row in ipairs(rows) do row.note=false end
  for _,note in ipairs(notes) do
    local selected, first = {}, nil
    for _,line in ipairs(note.rows) do selected[line] = true end
    for i,row in ipairs(rows) do
      if selected[row.source] then row.note=true; first=first or i-1 end
    end
    if first then
      local win = vim.fn.bufwinid(target)
      local available = api.nvim_win_get_width(win ~= -1 and win or main_window)-number_width-4
      local width = math.max(10,math.min(64,math.floor(available*0.75)))
      local body_width = width-6
      local lines, widest = {}, 0
      for _,line in ipairs(vim.split(note.comment,'\n',{plain=true})) do
        local part = ''
        for _,char in ipairs(vim.fn.split(line:gsub('\t','    '),'\\zs')) do
          if vim.fn.strdisplaywidth(part..char) > body_width and part ~= '' then
            local prefix, suffix = part:match('^(.*)%s([^%s]*)$')
            local wrapped = prefix and prefix ~= '' and prefix or part
            lines[#lines+1] = wrapped
            widest = math.max(widest,vim.fn.strdisplaywidth(wrapped))
            part = wrapped == part and '' or suffix
          end
          part = part..char
        end
        lines[#lines+1] = part
        widest = math.max(widest,vim.fn.strdisplaywidth(part))
      end
      width = math.min(width,math.max(28,widest+6))
      local callout = {{{' ╭─ NOTE '..string.rep('─',width-9)..'╮','RxNoteLabel'}}}
      local function body(text)
        callout[#callout+1] = {{' │','RxNoteLabel'},
          {'  '..text..string.rep(' ',math.max(0,width-4-vim.fn.strdisplaywidth(text))),'RxNoteText'}, {'│','RxNoteLabel'}}
      end
      body('')
      for _,line in ipairs(lines) do body(line) end
      body('')
      local gutter = number_width+4
      for _,line in ipairs(callout) do table.insert(line,1,{string.rep(' ',gutter),'Normal'}) end
      callout[#callout+1] = {{string.rep(' ',gutter+width+1),'RxNoteJoin'}}
      api.nvim_buf_set_extmark(target,notes_ns,first,0,{virt_lines=callout,virt_lines_above=true,virt_lines_leftcol=true})
    end
  end
end

function _G.RexDiffNotes(notes)
  note_data = notes
  for target,rows in pairs(views) do notes_for_buffer(target,rows,notes) end
  vim.cmd('redraw!')
end
