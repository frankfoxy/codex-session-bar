use std::{borrow::Cow,sync::atomic::{AtomicU16,AtomicBool,Ordering}};
static CULTURE:AtomicU16=AtomicU16::new(0x409);
static CODEX:AtomicBool=AtomicBool::new(false);
pub fn chinese()->bool {CULTURE.load(Ordering::Relaxed)&0x3ff==4}
pub fn follow(culture:u16) {if culture!=0 {CULTURE.store(culture,Ordering::Relaxed);CODEX.store(true,Ordering::Relaxed);}}
pub fn initialize(){unsafe{CULTURE.store(windows::Win32::Globalization::GetUserDefaultUILanguage(),Ordering::Relaxed);}CODEX.store(false,Ordering::Relaxed);}
pub fn status()->(u16,bool){(CULTURE.load(Ordering::Relaxed),CODEX.load(Ordering::Relaxed))}
pub fn text(text:&str)->Cow<'_,str>{translate(text,chinese())}
pub fn translate(text:&str,zh:bool)->Cow<'_,str>{
    if !zh {return if text=="默认集合"{Cow::Borrowed("Default collection")}else{Cow::Borrowed(text)};}
    if let Some((symbol,status))=text.split_once(' ') {
        if matches!(symbol,"●"|"○"|"✓"|"◷"|"Ⅱ"|"!"){return Cow::Owned(format!("{symbol} {}",translate(status,true)));}
    }
    if let Some(count)=text.strip_suffix(" chats  ·  Scroll to browse  ·  Enter to select") {return Cow::Owned(format!("{count} 个会话 · 滚动浏览 · 回车选择"));}
    for (prefix,translated) in [("Copy failed: ","复制失败："),("Action failed: ","操作失败："),("Account limits: ","账户额度："),("Cannot change protection: ","无法修改防护设置："),("Cannot read protection log: ","无法读取防护日志："),("Could not save: ","保存失败：")] {
        if let Some(tail)=text.strip_prefix(prefix){return Cow::Owned(format!("{translated}{}",translate(tail,true)));}
    }
    if text.contains(" · ended "){return Cow::Owned(text.replace(" · ended "," · 已结束 "));}
    let translated=match text {
        "Settings"=>"设置","Refresh settings"=>"刷新设置","Recent chats"=>"最近会话","Session details"=>"会话详情",
        "Check for updates"=>"检查更新","Checking…"=>"正在检查…",
        "Reset announcements"=>"官方重置预告",
        "Reset announcement checks are disabled"=>"重置预告检查已关闭",
        "Follow published Codex reset announcements · selected source only"=>"跟踪公开的 Codex 重置公告 · 只检查选中的来源",
        "Announcement check interval · 1–60 minutes · changes apply immediately"=>"公告检查间隔 · 1–60 分钟 · 修改后立即生效",
        "Check GitHub for updates now · ask before downloading"=>"立即检查 GitHub 更新 · 下载前会询问确认",
        "Protection log"=>"防护日志","Session icon"=>"会话图标","Choose a fixed chat"=>"选择固定监看的会话",
        "Protection limits"=>"防护阈值","Process limit"=>"进程数量上限","Total private memory"=>"合计私有内存上限","Normal check interval"=>"正常检查间隔",
        "Protect Codex Git · configurable checks · 1s after trigger"=>"Codex Git 防护 · 检查间隔可配置 · 触发后每秒检查",
        "Protection limits and check interval"=>"配置防护阈值和检查间隔","Adjust protection limit · apply to save"=>"调整防护阈值 · 应用后保存",
        "After a trigger: 1s checks until 60s stable."=>"触发后每秒检查，持续稳定 60 秒后恢复正常间隔。",
        "Alerts and recording cannot pause enforcement."=>"告警弹框和内存采样不会阻塞防护。",
        "Only verified Codex read-only Git queries are ended."=>"只结束确认由 Codex 启动的只读 Git 查询进程。",
        "Copy the conversation text as Markdown"=>"将会话正文复制为 Markdown","Copied"=>"已复制","Copied Markdown"=>"已复制 Markdown","Preparing Markdown…"=>"正在生成 Markdown…",
        "Protection is off"=>"防护已关闭","Starting Git protection…"=>"正在启动 Git 防护…",
        "Applying change…"=>"正在应用修改…","Git protection is starting or unavailable"=>"Git 防护正在启动或暂不可用",
        "No chat selected · showing all Busy chats"=>"未选择会话 · 显示全部忙碌会话","Loading chat metrics…"=>"正在加载会话统计…",
        "Searching chat bodies…"=>"正在检索会话正文…","Ask anything"=>"输入问题","Limit"=>"上限",
        "No local chat messages available"=>"没有可用的本机会话正文","Local chat history unavailable"=>"本机会话正文记录不可用",
        "Chat too large to copy safely"=>"会话超过单次复制上限（16 MiB / 20000 条），未修改剪贴板",
        "Chat message too large to copy safely"=>"单条消息超过复制上限（4 MiB），未修改剪贴板",
        "Shortcut unavailable; the previous shortcut is unchanged"=>"快捷键不可用，已保留原快捷键",
        "Use Ctrl or Alt with a letter, number or F1–F24"=>"请使用 Ctrl 或 Alt 搭配字母、数字或 F1–F24",
        "Chat tags"=>"会话标签","Project / group · synced, read-only"=>"项目 / 分组 · 同步，只读",
        "Create your first custom tag below"=>"在下方创建自定义标签","Add or select a tag…"=>"添加或选择标签…",
        "Changes are saved locally"=>"修改保存在本机","Saved locally"=>"已保存到本机","Saved"=>"已保存",
        "Change chat icon"=>"更改会话图标","Drag session bar"=>"拖动会话条",
        "Hide to tray · restore from the tray icon or recent-chats shortcut"=>"隐藏到托盘 · 点击托盘图标或使用会话列表快捷键恢复",
        "Settings · list size, shortcut and extensions"=>"设置 · 列表大小、快捷键与扩展功能",
        "Show fewer rows"=>"减少显示行数","Show more rows"=>"增加显示行数",
        "Enable shortcut · only while Codex or Session Bar has focus"=>"启用快捷键 · 仅在 Codex 或会话条获得焦点时有效",
        "Apply and save settings"=>"应用并保存设置","Restore default list size and shortcut · apply to save"=>"恢复默认列表大小和快捷键 · 应用后保存",
        "Open chat in Codex · blue dot = newly completed, unread result"=>"在 Codex 中打开会话 · 蓝点表示新完成且未读",
        "Use this fixed chat · blue dot = newly completed, unread result"=>"固定监看此会话 · 蓝点表示新完成且未读",
        "Back to session settings"=>"返回会话设置","Random icon"=>"随机图标","Remove chat icon"=>"移除会话图标",
        "Suggest icon from chat title · local matching"=>"根据会话标题推荐图标 · 本机匹配",
        "Dock in the blank title bar area"=>"停靠到标题栏空白区域",
        "No recent events · open the folder for archived logs"=>"没有最近记录 · 可打开目录查看归档日志",
        "Episode counter is not available yet"=>"防护次数暂不可用",
        "View protection log · badge counts storm episodes, not processes · new tracking; recovered after 60 stable seconds"=>"查看防护日志 · 角标记录风暴次数 · 稳定 60 秒后视为恢复",
        "Visible rows"=>"显示行数","Title column width"=>"名称列最大宽度","Global shortcut"=>"会话列表快捷键","Open recent chats"=>"打开会话列表",
        "Extensions"=>"扩展功能","Current chat minimum"=>"当前会话最小间隔","Chat list minimum"=>"会话列表最小间隔",
        "Fallback refresh"=>"低频兜底检查","Cache save interval"=>"缓存最小写入间隔","Diagnostic status"=>"诊断状态输出间隔","Window identity check"=>"窗口识别检查间隔",
        "File-change events · debounced · slow fallback"=>"异步文件通知 · 合并事件 · 低频兜底",
        "File notifications unavailable · fallback active"=>"文件通知暂不可用 · 已使用低频兜底",
        "Minimums coalesce events; unchanged chats wait for fallback."=>"合并密集事件；无变化时只进行低频兜底检查。",
        "Window following and typing stay responsive."=>"窗口跟随与输入操作仍保持及时响应。",
        "Click the field, then press Ctrl / Alt + a letter, number or F-key"=>"点击输入框，按 Ctrl / Alt 加字母、数字或功能键",
        "Apply to save changes"=>"点击应用以保存修改","Saved · changes are active"=>"已保存 · 修改已生效","Defaults restored · apply to save"=>"已恢复默认值 · 应用后保存",
        "Follow current chat"=>"跟随当前会话","Choose a fixed chat…"=>"选择固定监看的会话…","Choose a fixed chat..."=>"选择固定监看的会话…",
        "Projects"=>"项目","Groups"=>"分组","Custom tags"=>"自定义标签","Project"=>"项目","Group"=>"分组","Custom"=>"自定义","Tags"=>"标签",
        "Name · pinyin · initials"=>"名称 · 全拼 · 拼音首字母","No matching chats"=>"没有匹配的会话","No matching tags"=>"没有匹配的标签",
        "No matching emoji"=>"没有匹配的图标","Search all emoji…"=>"搜索全部图标…","All emoji"=>"全部图标",
        "Smileys & Emotion"=>"表情与情绪","People & Body"=>"人物与身体","Animals & Nature"=>"动物与自然","Food & Drink"=>"食物与饮品",
        "Travel & Places"=>"旅行与地点","Activities"=>"活动","Objects"=>"物品","Symbols"=>"符号","Flags"=>"旗帜","Component"=>"组件",
        "Copy deeplink"=>"复制会话链接","Copy as Markdown"=>"复制为 Markdown","Copy working directory"=>"复制工作目录",
        "Open session bar"=>"打开会话条","Hide to tray"=>"隐藏到托盘","Quit session bar"=>"退出会话条","Quit Session Bar"=>"退出会话条",
        "Chat opened in Codex"=>"已在 Codex 中打开会话","Connecting…"=>"正在连接…","Switching…"=>"正在切换…","New chat"=>"新会话",
        "Regenerate chat name"=>"重新生成会话名称","Clear"=>"清除","Back"=>"返回","Busy"=>"忙碌","Done"=>"已完成","Waiting"=>"等待中","Stopped"=>"已停止","Failed"=>"失败","Ready"=>"就绪","Unknown"=>"未知",
        "Codex Git memory protection"=>"Codex Git 内存防护",
        "Refresh account limits and chat list"=>"刷新账户额度与会话列表","Dock in title bar"=>"停靠到标题栏",
        "Open settings · shortcuts, recent list and extensions"=>"打开设置 · 快捷键、会话列表与扩展功能",
        "Follow the current Codex chat"=>"跟随当前 Codex 会话","Back to settings"=>"返回设置",
        "Refresh settings · source events, rate limits, fallback and diagnostics"=>"刷新设置 · 事件通知、合并间隔、兜底与诊断",
        "Back to settings · keep unsaved changes"=>"返回设置 · 保留未保存的修改",
        "Increase refresh interval · apply to save"=>"增大刷新间隔 · 应用后保存","Decrease refresh interval · apply to save"=>"减小刷新间隔 · 应用后保存",
        "Search message text · off by default"=>"检索会话正文 · 默认关闭","Clear chat search · keep tag filters"=>"清除会话搜索 · 保留标签筛选",
        "Clear tag search · keep selected tags"=>"清除标签搜索 · 保留已选标签","Clear emoji search · keep category"=>"清除图标搜索 · 保留分类",
        "Title column maximum · 140–280 DIP · two-line titles and previews"=>"名称列最大宽度 · 140–280 DIP · 名称与摘要最多两行",
        "Compact view · one line per chat, hide tags and prompt preview"=>"紧凑模式 · 每条仅一行，隐藏标签与提问摘要",
        "Tag filters · expand or collapse · search names or pinyin"=>"标签筛选 · 展开或收起 · 支持名称和拼音",
        "Filter by this tag · selected tags must all match"=>"按此标签筛选 · 需同时满足全部已选标签","Clear tag filters"=>"清除标签筛选",
        "Show all tags · scroll for more · keep the chat list open"=>"显示全部标签 · 可滚动 · 保持会话列表打开",
        "Close all tags · keep the chat list open"=>"关闭全部标签面板 · 保持会话列表打开","All tags assigned to this chat"=>"此会话的全部标签",
        "Edit custom tags · project and group tags are read-only"=>"编辑自定义标签 · 项目与分组标签只读",
        "Add or remove this custom tag from the chat"=>"为会话添加或移除此自定义标签","Create or attach the tag"=>"新建或添加标签",
        "Choose custom tag color"=>"选择自定义标签颜色","Close tag editor · keep the chat list open"=>"关闭标签编辑 · 保持会话列表打开",
        "Copy a link to this chat"=>"复制指向此会话的链接","Copy the chat title and link as Markdown"=>"将会话标题和链接复制为 Markdown",
        "Copy the chat working directory"=>"复制会话的工作目录","Close menu · keep the chat list open"=>"关闭菜单 · 保持会话列表打开","Chat copy actions"=>"会话复制操作",
        "Refresh recent protection events"=>"刷新最近的防护记录","Open log folder · includes compressed history"=>"打开日志目录 · 包含压缩归档",
        "Protect Codex Git · every 5 seconds · alerts and automatic cleanup"=>"防护 Codex Git · 每 5 秒检查 · 告警并清理异常进程",
        "Apply settings"=>"应用设置","Reset settings"=>"恢复默认设置",
        _=>{
            let replacements=[("Recent chats · ","最近会话 · "),("recent chats · hide from settings or tray","最近会话 · 在设置或托盘中隐藏"),("recorded storm episodes · view log · continuous storms count once; recovered after 60 stable seconds","次风暴记录 · 点击查看日志 · 持续风暴只计一次，稳定 60 秒后视为恢复"),("newly completed chats · blue dot = unread result · open chat to mark as read","条新完成的会话 · 蓝点表示未读 · 打开会话后标记为已读"),("session tokens:","会话 Token："),("Session tokens:","会话 Token："),("Output:","输出："),(" chats"," 个会话"),(" tags"," 个标签"),(" · Index "," · 索引 "),("Index ","索引 "),("Indexed ","已索引 "),(" · Total "," · 总计 "),("Total ","总计 "),("Busy (","忙碌 ("),("● Busy","● 忙碌"),("○ Busy","○ 忙碌"),("✓ Done","✓ 已完成"),("○ Unknown","○ 未知"),("○ Ready","○ 就绪"),("● Waiting","● 等待中"),("All tags · ","全部标签 · "),(" · new"," · 新完成"),(" new"," 条新完成"),("synced, read-only","同步标签，只读"),("custom tag","自定义标签"),("click to toggle filter","点击切换筛选"),(" icons · scroll for more"," 个图标 · 滚动查看更多"),(" results · all emoji"," 个结果 · 全部图标"),(" · retry pending"," · 等待重试"),("% left","% 剩余"),("Copy failed:","复制失败："),("Action failed:","操作失败："),("Could not save:","保存失败："),("Episode tracking since ","防护计数开始于 "),(" · earlier alarms are not counted"," · 不包含此前告警"),(" recent events · scroll · latest log tail"," 条最近记录 · 滚动查看"),("Storms ","风暴次数 "),("Ended ","已结束 "),(" processes"," 个进程"),("Unavailable","不可用"),("Protection off","防护已关闭"),("Stable","稳定"),("Active","正在防护"),("blocked ","已阻止 ")];
            let mut value=Cow::Borrowed(text);
            for(from,to)in replacements {if from!=to && value.contains(from){value=Cow::Owned(value.replace(from,to));}}
            return value;
        }
    };
    Cow::Borrowed(translated)
}
#[cfg(test)]mod tests {
    use super::*;
    #[test]fn language_changes_only_interface_strings(){
        assert_eq!(translate("Copy working directory",true),"复制工作目录");
        assert_eq!(translate("Copy working directory",false),"Copy working directory");
        assert_eq!(translate("默认集合",false),"Default collection");
        assert_eq!(translate("相机 [联调]",true),"相机 [联调]");
        assert!(translate("40 chats · Index 100%",true).contains("40 个会话"));
        assert!(translate("Copy failed: No local chat messages available",true).contains("没有可用"));
        assert_eq!(translate("0 Git · 0 MiB · ended 128",true),"0 Git · 0 MiB · 已结束 128");
        assert_eq!(translate("◷ Waiting",true),"◷ 等待中");
        assert_eq!(translate("Ⅱ Stopped",true),"Ⅱ 已停止");
        assert_eq!(translate("! Failed",true),"! 失败");
    }
}
