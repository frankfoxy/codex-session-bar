//! A bounded Markdown projection for hover previews. Source text remains untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct Inline { pub text:String, pub bold:Vec<(u32,u32)>, pub code:Vec<(u32,u32)> }
#[derive(Debug, Clone, PartialEq)]
pub enum Block { Text(Inline, bool), Code(String), Table(Vec<Vec<Inline>>) }
fn span_len(s:&str)->u32{s.encode_utf16().count() as u32}
pub fn inline(s:&str)->Inline {
    let mut out=Inline{text:String::new(),bold:Vec::new(),code:Vec::new()};let mut rest=s;
    while !rest.is_empty(){
        let pair=if rest.starts_with("**"){Some(("**",true))}else if rest.starts_with("__"){Some(("__",true))}else if rest.starts_with('`'){Some(("`",false))}else{None};
        if let Some((delimiter,bold))=pair {let tail=&rest[delimiter.len()..];if let Some(end)=tail.find(delimiter){let start=span_len(&out.text);let value=&tail[..end];out.text.push_str(value);let n=span_len(value);if bold{out.bold.push((start,n))}else{out.code.push((start,n))}rest=&tail[end+delimiter.len()..];continue}}
        if rest.starts_with('[') {if let Some(i)=rest.find("]("){if let Some(j)=rest[i+2..].find(')'){out.text.push_str(&rest[1..i]);rest=&rest[i+3+j..];continue}}}
        let c=rest.chars().next().unwrap();out.text.push(c);rest=&rest[c.len_utf8()..];
    } out
}
fn cells(line:&str)->Vec<Inline>{line.trim().trim_matches('|').split('|').take(12).map(|s|inline(s.trim())).collect()}
fn separator(line:&str)->bool {let c:Vec<_>=line.trim().trim_matches('|').split('|').collect();!c.is_empty()&&c.iter().all(|s| {let t=s.trim().trim_matches(':');t.len()>=3&&t.chars().all(|x|x=='-')})}
pub fn parse(text:&str)->Vec<Block>{
 let lines:Vec<_>=text.lines().take(512).collect();let mut result=Vec::new();let mut i=0;
 while i<lines.len(){let line=lines[i];let t=line.trim();
  if t.starts_with("```") {i+=1;let mut code=String::new();while i<lines.len()&&!lines[i].trim().starts_with("```"){if !code.is_empty(){code.push('\n')}code.push_str(lines[i]);i+=1}result.push(Block::Code(code));if i<lines.len(){i+=1}continue}
  if i+1<lines.len()&&t.contains('|')&&separator(lines[i+1]){let mut rows=vec![cells(line)];i+=2;while i<lines.len()&&lines[i].contains('|')&&!lines[i].trim().is_empty(){rows.push(cells(lines[i]));i+=1}result.push(Block::Table(rows));continue}
  if t.is_empty(){result.push(Block::Text(inline(""),false));i+=1;continue}
  let n=t.chars().take_while(|c|*c=='#').count();let heading=(1..=6).contains(&n)&&t.as_bytes().get(n)==Some(&b' ');let content=if heading {t[n..].trim_start()}else if let Some(v)=t.strip_prefix("- ").or_else(||t.strip_prefix("* ")){let item=format!("• {v}");result.push(Block::Text(inline(&item),false));i+=1;continue}else{t};
  result.push(Block::Text(inline(content),heading));i+=1;
 } result
}
#[cfg(test)] mod tests { use super::*; #[test] fn formats_tables_and_inline_unicode(){let b=parse("# 标题\n**资源** `XLR`\n| 名称 | 数值 |\n| --- | ---: |\n| foo | 42 |");assert!(matches!(&b[0],Block::Text(_,true)));if let Block::Text(v,_)=&b[1]{assert_eq!(v.text,"资源 XLR");assert_eq!(v.bold,vec![(0,2)]);assert_eq!(v.code,vec![(3,3)]);}else{panic!()}if let Block::Table(rows)=&b[2]{assert_eq!(rows.len(),2);assert_eq!(rows[1][1].text,"42")}else{panic!()}} }
