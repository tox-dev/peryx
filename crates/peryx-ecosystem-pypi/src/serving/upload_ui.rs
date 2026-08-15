use std::fmt::Write as _;

use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse as _, Response};
use peryx_driver::AppState;

pub(super) fn response(state: &AppState) -> Response {
    let mut html = String::from(HEAD);
    for index in state
        .serving
        .describe_indexes()
        .into_iter()
        .filter(|index| index.ecosystem == crate::ECOSYSTEM.as_str() && index.uploads)
    {
        let route = escape(&index.route);
        let name = escape(&index.name);
        write!(html, "<option value=\"/{route}/\">{name} ({route})</option>").expect("write to string");
    }
    html.push_str(TAIL);
    let mut response = Html(html).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const HEAD: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>PyPI upload</title><style>body{font:16px/1.5 sans-serif;max-width:48rem;margin:4rem auto;padding:0 1rem}form{display:grid;gap:1rem}label{display:grid;gap:.35rem}button{width:max-content}progress{width:100%}</style></head><body><main><h1>PyPI upload</h1><p>Choose one wheel or source distribution. The repository size limit applies.</p><form id="upload"><label>Repository<select id="repository" required>"#;

const TAIL: &str = r#"</select></label><label>Upload token<input id="token" type="password" autocomplete="off" required></label><label>Distribution<input id="artifact" type="file" name="content" accept=".whl,.tar.gz" required></label><div><button id="submit" type="submit">Upload</button> <button id="cancel" type="button" disabled>Cancel</button></div><progress id="progress" value="0" max="100"></progress><output id="outcome"></output></form></main><script>'use strict';const form=document.querySelector('#upload'),repository=document.querySelector('#repository'),token=document.querySelector('#token'),artifact=document.querySelector('#artifact'),submit=document.querySelector('#submit'),cancel=document.querySelector('#cancel'),progress=document.querySelector('#progress'),outcome=document.querySelector('#outcome');let request=null;const finish=()=>{request=null;submit.disabled=false;cancel.disabled=true};form.addEventListener('submit',event=>{event.preventDefault();const file=artifact.files[0],name=(file?.name||'').toLowerCase();if(!file||(!name.endsWith('.whl')&&!name.endsWith('.tar.gz'))){outcome.value='Choose one wheel or .tar.gz source distribution.';return}request=new XMLHttpRequest();request.open('POST',repository.value,true);request.setRequestHeader('authorization','Basic '+btoa('__token__:'+token.value));request.setRequestHeader('x-peryx-csrf',location.origin);request.upload.onprogress=event=>{if(event.lengthComputable)progress.value=Math.min(100,event.loaded/event.total*100)};request.onloadend=()=>{const body=(request.responseText||'').trim().slice(0,512),success=request.status>=200&&request.status<300;outcome.value=success?file.name+': uploaded':request.status>=500?file.name+': server could not store the upload':body||file.name+': upload failed ('+request.status+')';progress.value=request.status<400?100:0;finish()};request.onerror=()=>{outcome.value=file.name+': connection failed';progress.value=0;finish()};submit.disabled=true;cancel.disabled=false;outcome.value=file.name+': uploading';progress.value=0;const data=new FormData();data.append('content',file,file.name);request.send(data)});cancel.addEventListener('click',()=>{if(request){request.abort();outcome.value='Upload cancelled.';progress.value=0;finish()}});</script></body></html>"#;

#[cfg(test)]
#[path = "../../tests/unit/serving/upload_ui/tests.rs"]
mod tests;
