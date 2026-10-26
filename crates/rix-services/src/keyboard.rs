use anyhow::Result;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub code: String,
    pub variant: String,
    pub name: String,
}
pub fn catalog() -> Result<Vec<Layout>> {
    let root = std::env::var_os("XKB_CONFIG_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/usr/share/X11/xkb".into());
    parse(&std::fs::read_to_string(root.join("rules/evdev.xml"))?)
}
fn parse(xml: &str) -> Result<Vec<Layout>> {
    let doc = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )?;
    let text = |node: roxmltree::Node<'_, '_>, name| {
        node.children()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .unwrap_or_default()
            .to_owned()
    };
    let mut result = Vec::new();
    for layout in doc.descendants().filter(|n| n.has_tag_name("layout")) {
        let Some(config) = layout.children().find(|n| n.has_tag_name("configItem")) else {
            continue;
        };
        let code = text(config, "name");
        let name = text(config, "description");
        if code.is_empty() {
            continue;
        }
        result.push(Layout {
            code: code.clone(),
            variant: String::new(),
            name: name.clone(),
        });
        for variant in layout.descendants().filter(|n| n.has_tag_name("variant")) {
            if let Some(config) = variant.children().find(|n| n.has_tag_name("configItem")) {
                result.push(Layout {
                    code: code.clone(),
                    variant: text(config, "name"),
                    name: text(config, "description"),
                });
            }
        }
    }
    result.sort_by_key(|l| l.name.to_lowercase());
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_variants_with_parent_layout() {
        let rows = parse("<!DOCTYPE xkbConfigRegistry SYSTEM \"xkb.dtd\"><xkbConfigRegistry><layoutList><layout><configItem><name>us</name><description>English</description></configItem><variantList><variant><configItem><name>dvorak</name><description>Dvorak</description></configItem></variant></variantList></layout></layoutList></xkbConfigRegistry>").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].code, "us");
        assert_eq!(rows[0].variant, "dvorak");
    }
}
