use super::*;
use agent_client_protocol::schema::v1::{ImageContent, ResourceLink, TextContent};

#[test]
fn blocs_vers_texte_et_images() {
    let blocks = vec![
        ContentBlock::Text(TextContent::new("bonjour")),
        ContentBlock::ResourceLink(ResourceLink::new("fichier", "file:///etc/hosts")),
        ContentBlock::Image(ImageContent::new("aGVsbG8=", "image/png")),
    ];
    let (text, images) = blocks_to_parts(&blocks);
    assert!(text.contains("bonjour"));
    assert!(text.contains("file:///etc/hosts"));
    assert!(text.contains("image jointe"));
    assert_eq!(images, vec![("aGVsbG8=".to_string(), "image/png".to_string())]);
}
