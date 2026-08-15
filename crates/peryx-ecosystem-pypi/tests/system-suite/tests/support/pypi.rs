use peryx_storage::blob::Digest;

use crate::harness::Node;

pub const UPLOAD_TOKEN: &str = "harness-upload-secret";
pub const WHEEL: &[u8] = include_bytes!("../fixtures/veloxdemo-1.0.0-py3-none-any.whl");
pub const WHEEL_FILENAME: &str = "veloxdemo-1.0.0-py3-none-any.whl";

pub const fn config() -> &'static str {
    "[[index]]\nname = \"hosted\"\necosystem = \"pypi\"\nhosted = true\nvolatile = true\n\n\
     [[index.access_token]]\nname = \"uploader\"\nsecret = \"harness-upload-secret\"\n\
     projects = [\"*\"]\nactions = [\"write\", \"delete\"]\n"
}

pub fn wheel_digest() -> String {
    Digest::of(WHEEL).as_str().to_owned()
}

pub trait PypiNodeExt {
    fn publish(&self) -> Result<(u16, String), reqwest::Error>;
    fn download_wheel(&self) -> Option<(u16, Vec<u8>)>;
}

impl PypiNodeExt for Node {
    fn publish(&self) -> Result<(u16, String), reqwest::Error> {
        let form = reqwest::blocking::multipart::Form::new()
            .text(":action", "file_upload")
            .text("name", "veloxdemo")
            .text("version", "1.0.0")
            .text("filetype", "bdist_wheel")
            .text("sha256_digest", wheel_digest())
            .part(
                "content",
                reqwest::blocking::multipart::Part::bytes(WHEEL.to_vec()).file_name(WHEEL_FILENAME),
            );
        let response = self
            .request(reqwest::Method::POST, "/hosted/")
            .basic_auth("__token__", Some(UPLOAD_TOKEN))
            .multipart(form)
            .send()?;
        let code = response.status().as_u16();
        Ok((code, response.text().unwrap_or_default()))
    }

    fn download_wheel(&self) -> Option<(u16, Vec<u8>)> {
        self.download(&format!("/hosted/files/{}/{WHEEL_FILENAME}", wheel_digest()))
    }
}

pub fn publish(node: &Node) -> Result<(u16, String), reqwest::Error> {
    node.publish()
}

pub fn download_wheel(node: &Node) -> Option<(u16, Vec<u8>)> {
    node.download_wheel()
}
