//! 按元素修改 XML，保留未知属性、元素、注释；缺少安全字段时补齐。
use super::{error, home::HomeSpec};
use crate::error::AppError;
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};

struct Element {
    start: BytesStart<'static>,
    children: Vec<Node>,
}
enum Node {
    Element(Element),
    Other(Event<'static>),
}
impl Element {
    fn named(&self, name: &str) -> bool {
        self.start.name().as_ref() == name
    }
    fn new(name: &str) -> Self {
        Self {
            start: BytesStart::new(name.to_owned()),
            children: vec![],
        }
    }
    fn child(&mut self, name: &str) -> &mut Element {
        let index = self
            .children
            .iter()
            .position(|n| matches!(n, Node::Element(e) if e.named(name)));
        let index = index.unwrap_or_else(|| {
            self.children.push(Node::Element(Self::new(name)));
            self.children.len() - 1
        });
        match &mut self.children[index] {
            Node::Element(e) => e,
            _ => unreachable!(),
        }
    }
    fn set(&mut self, name: &str, value: &str) {
        self.children
            .retain(|n| !matches!(n, Node::Element(e) if e.named(name)));
        let mut child = Self::new(name);
        child
            .children
            .push(Node::Other(Event::Text(BytesText::new(value).into_owned())));
        self.children.push(Node::Element(child));
    }
    fn attr(&mut self, name: &str, value: &str) -> Result<(), AppError> {
        let mut attrs = vec![];
        for attr in self.start.attributes() {
            let attr = attr.map_err(|_| {
                error(
                    "syncthing.invalid_configuration_xml_attribute",
                    "配置 XML 属性无效",
                )
            })?;
            if attr.key.as_ref() != name {
                attrs.push((attr.key.as_ref().to_owned(), attr.value.into_owned()));
            }
        }
        self.start.clear_attributes();
        for (key, val) in &attrs {
            self.start
                .push_attribute(quick_xml::events::attributes::Attribute {
                    key: quick_xml::name::QName(key),
                    value: std::borrow::Cow::Borrowed(val),
                });
        }
        self.start.push_attribute((name, value));
        Ok(())
    }
    fn write(self, writer: &mut Writer<Vec<u8>>) -> Result<(), AppError> {
        let name = self.start.name().as_ref().to_owned();
        writer
            .write_event(Event::Start(self.start))
            .map_err(|_| error("syncthing.couldn_t_write_xml", "无法写入 XML"))?;
        for node in self.children {
            match node {
                Node::Element(e) => e.write(writer)?,
                Node::Other(event) => writer
                    .write_event(event)
                    .map_err(|_| error("syncthing.couldn_t_write_xml", "无法写入 XML"))?,
            }
        }
        writer
            .write_event(Event::End(BytesEnd::new(name)))
            .map_err(|_| error("syncthing.couldn_t_write_xml", "无法写入 XML"))?;
        Ok(())
    }
}
pub(super) fn harden(input: &[u8], spec: &HomeSpec) -> Result<Vec<u8>, AppError> {
    harden_with_key(input, spec, "")
}
pub(super) fn harden_with_key(
    input: &[u8],
    spec: &HomeSpec,
    api_key: &str,
) -> Result<Vec<u8>, AppError> {
    let nodes = parse(input)?;
    let mut writer = Writer::new(Vec::new());
    for node in nodes {
        match node {
            Node::Element(mut root) => {
                // 只去掉官方默认文件夹，已由用户添加的任务保持不变。
                root.children.retain(|n| !matches!(n, Node::Element(e) if e.named("folder") && e.start.attributes().flatten().any(|a| a.key.as_ref() == "id" && a.value.as_ref() == "default")));
                let gui = root.child("gui");
                let password_set = gui.children.iter().any(|n| matches!(n, Node::Element(e) if e.named("password") && e.children.iter().any(|n| matches!(n, Node::Other(Event::Text(t)) if !t.as_ref().is_empty()))));
                if !password_set {
                    return Err(error(
                        "syncthing.existing_configuration_has_no_gui_password_set_up_authentication_first",
                        "现有配置缺少 GUI 口令，请先设置认证",
                    ));
                }
                gui.set("user", &spec.gui_user);
                gui.set("metricsWithoutAuth", "false");
                gui.set("insecureSkipHostcheck", "false");
                gui.attr("tls", "false")?;
                gui.attr("enabled", "true")?;
                gui.set("address", &spec.gui.address()?);
                gui.set("apikey", api_key);
                gui.set("insecureAdminAccess", "false");
                let options = root.child("options");
                options.set(
                    "listenAddress",
                    &format!("tcp://127.0.0.1:{}", spec.listen_port),
                );
                for key in [
                    "globalAnnounceEnabled",
                    "localAnnounceEnabled",
                    "relaysEnabled",
                    "natEnabled",
                    "crashReportingEnabled",
                    "startBrowser",
                    "announceLANAddresses",
                ] {
                    options.set(key, "false");
                }
                options.set("autoUpgradeIntervalH", "0");
                options.set("urAccepted", "-1");
                // int32 最大值覆盖当前及后续统计问卷版本，不再弹出统计提示。
                options.set("urSeen", "2147483647");
                root.child("defaults")
                    .child("folder")
                    .set("maxConflicts", "-1");
                root.write(&mut writer)?;
            }
            Node::Other(e) => writer
                .write_event(e)
                .map_err(|_| error("syncthing.couldn_t_write_xml", "无法写入 XML"))?,
        }
    }
    Ok(writer.into_inner())
}

fn parse(input: &[u8]) -> Result<Vec<Node>, AppError> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Element> = vec![];
    let mut nodes = vec![];
    loop {
        match reader
            .read_event()
            .map_err(|_| error("syncthing.invalid_configuration_xml", "配置 XML 无效"))?
        {
            Event::Start(e) => {
                if stack.len() >= 128 {
                    return Err(error(
                        "syncthing.configuration_xml_is_nested_too_deeply",
                        "配置 XML 嵌套过深",
                    ));
                }
                if e.name().as_ref().contains(':') {
                    return Err(error(
                        "syncthing.configuration_xml_cannot_use_namespaces",
                        "配置 XML 不允许命名空间",
                    ));
                }
                for attr in e.attributes() {
                    let attr = attr.map_err(|_| {
                        error(
                            "syncthing.invalid_configuration_xml_attribute",
                            "配置 XML 属性无效",
                        )
                    })?;
                    if attr.key.as_ref().contains(':') || attr.key.as_ref() == "xmlns" {
                        return Err(error(
                            "syncthing.configuration_xml_cannot_use_namespaces",
                            "配置 XML 不允许命名空间",
                        ));
                    }
                }
                stack.push(Element {
                    start: e.into_owned(),
                    children: vec![],
                });
            }
            Event::End(_) => {
                let element = stack.pop().ok_or_else(|| {
                    error(
                        "syncthing.invalid_configuration_xml_hierarchy",
                        "配置 XML 层次无效",
                    )
                })?;
                let node = Node::Element(element);
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    nodes.push(node);
                }
            }
            Event::Eof => break,
            Event::DocType(_) => {
                return Err(error(
                    "syncthing.configuration_xml_cannot_contain_document_type_declarations",
                    "配置 XML 不允许文档类型声明",
                ));
            }
            e => {
                let n = Node::Other(e.into_owned());
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(n);
                } else {
                    nodes.push(n);
                }
            }
        }
    }
    if !stack.is_empty()
        || nodes
            .iter()
            .filter(|n| matches!(n, Node::Element(_)))
            .count()
            != 1
    {
        return Err(error(
            "syncthing.invalid_configuration_xml_root_element",
            "配置 XML 根元素无效",
        ));
    }
    for node in &nodes {
        if let Node::Element(root) = node {
            if !root.named("configuration") {
                return Err(error(
                    "syncthing.invalid_configuration_xml_root_element",
                    "配置 XML 根元素无效",
                ));
            }
            for name in ["gui", "options", "defaults"] {
                if root
                    .children
                    .iter()
                    .filter(|n| matches!(n, Node::Element(e) if e.named(name)))
                    .count()
                    > 1
                {
                    return Err(error(
                        "syncthing.configuration_xml_contains_duplicate_security_sections",
                        "配置 XML 存在重复安全区域",
                    ));
                }
            }
            for child in &root.children {
                if let Node::Element(defaults) = child
                    && defaults.named("defaults")
                {
                    for name in ["folder", "device"] {
                        if defaults
                            .children
                            .iter()
                            .filter(|n| matches!(n, Node::Element(e) if e.named(name)))
                            .count()
                            > 1
                        {
                            return Err(error(
                                "syncthing.configuration_xml_contains_duplicate_defaults_sections",
                                "配置 XML 存在重复默认区域",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(nodes)
}
pub(super) fn validate_source(input: &[u8]) -> Result<(), AppError> {
    parse(input).map(|_| ())
}
