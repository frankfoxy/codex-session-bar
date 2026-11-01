use std::{env,fs,path::PathBuf};
fn u16le(buf:&mut Vec<u8>,v:u16){buf.extend_from_slice(&v.to_le_bytes())}
fn u32le(buf:&mut Vec<u8>,v:u32){buf.extend_from_slice(&v.to_le_bytes())}
fn resource(out:&mut Vec<u8>,kind:u16,name:u16,data:&[u8]) {
    while out.len()%4!=0{out.push(0)}
    u32le(out,data.len() as u32);u32le(out,32);
    u16le(out,0xffff);u16le(out,kind);u16le(out,0xffff);u16le(out,name);
    u32le(out,0);u16le(out,if kind==0{0}else{0x1030});u16le(out,if kind==0{0}else{0x409});u32le(out,0);u32le(out,0);
    out.extend_from_slice(data);while out.len()%4!=0{out.push(0)}
}
fn icon_resource(ico:&[u8])->Vec<u8>{
    assert!(ico.len()>=6 && ico[0..4]==[0,0,1,0],"Invalid ICO header");
    let n=u16::from_le_bytes([ico[4],ico[5]]) as usize;assert!(n>0 && ico.len()>=6+n*16,"Invalid ICO directory");
    let mut res=Vec::new();resource(&mut res,0,0,&[]);let mut group=vec![0,0,1,0];u16le(&mut group,n as u16);
    for i in 0..n {let e=&ico[6+i*16..6+(i+1)*16];let len=u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;let off=u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;assert!(off.checked_add(len).is_some_and(|end|end<=ico.len()),"Invalid ICO image");resource(&mut res,3,(i+1) as u16,&ico[off..off+len]);group.extend_from_slice(&e[..12]);u16le(&mut group,(i+1) as u16);}
    resource(&mut res,14,1,&group);res
}
fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/session-bar.ico");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}\\app.manifest", env::var("CARGO_MANIFEST_DIR").unwrap());
        let ico=fs::read("assets/session-bar.ico").expect("Read application icon");let out=PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("session-bar-icon.res");fs::write(&out,icon_resource(&ico)).expect("Write icon resources");println!("cargo:rustc-link-arg={}",out.display());
    }
}
