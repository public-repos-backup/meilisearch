mod context;
mod document;
pub(crate) mod error;
mod fields;
mod template_checker;

use std::convert::TryFrom;

use error::{NewPromptError, RenderPromptError};

use self::context::Context;
use self::document::Document;
use crate::update::del_add::DelAdd;
use crate::FieldsIdsMap;

pub struct Prompt {
    template: liquid::Template,
    template_text: String,
    strategy: PromptFallbackStrategy,
    fallback: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PromptData {
    pub template: String,
    pub strategy: PromptFallbackStrategy,
    pub fallback: String,
}

impl From<Prompt> for PromptData {
    fn from(value: Prompt) -> Self {
        Self { template: value.template_text, strategy: value.strategy, fallback: value.fallback }
    }
}

impl TryFrom<PromptData> for Prompt {
    type Error = NewPromptError;

    fn try_from(value: PromptData) -> Result<Self, Self::Error> {
        Prompt::new(value.template, Some(value.strategy), Some(value.fallback))
    }
}

impl Clone for Prompt {
    fn clone(&self) -> Self {
        let template_text = self.template_text.clone();
        Self {
            template: new_template(&template_text).unwrap(),
            template_text,
            strategy: self.strategy,
            fallback: self.fallback.clone(),
        }
    }
}

fn new_template(text: &str) -> Result<liquid::Template, liquid::Error> {
    liquid::ParserBuilder::with_stdlib().build().unwrap().parse(text)
}

fn default_template() -> liquid::Template {
    new_template(default_template_text()).unwrap()
}

fn default_template_text() -> &'static str {
    "{% for field in fields %} \
    {{ field.name }}: {{ field.value }}\n\
    {% endfor %}"
}

fn default_fallback() -> &'static str {
    "<MISSING>"
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            template: default_template(),
            template_text: default_template_text().into(),
            strategy: Default::default(),
            fallback: default_fallback().into(),
        }
    }
}

impl Default for PromptData {
    fn default() -> Self {
        Self {
            template: default_template_text().into(),
            strategy: Default::default(),
            fallback: default_fallback().into(),
        }
    }
}

impl Prompt {
    pub fn new(
        template: String,
        strategy: Option<PromptFallbackStrategy>,
        fallback: Option<String>,
    ) -> Result<Self, NewPromptError> {
        let this = Self {
            template: liquid::ParserBuilder::with_stdlib()
                .build()
                .unwrap()
                .parse(&template)
                .map_err(NewPromptError::cannot_parse_template)?,
            template_text: template,
            strategy: strategy.unwrap_or_default(),
            fallback: fallback.unwrap_or_default(),
        };

        // render template with special object that's OK with `doc.*` and `fields.*`
        this.template
            .render(&template_checker::TemplateChecker)
            .map_err(NewPromptError::invalid_fields_in_template)?;

        Ok(this)
    }

    pub fn render(
        &self,
        document: obkv::KvReaderU16<'_>,
        side: DelAdd,
        field_id_map: &FieldsIdsMap,
    ) -> Result<String, RenderPromptError> {
        let document = Document::new(document, side, field_id_map);
        let context = Context::new(&document, field_id_map);

        self.template.render(&context).map_err(RenderPromptError::missing_context)
    }
}

#[derive(
    Debug, Default, Clone, PartialEq, Eq, Copy, serde::Serialize, serde::Deserialize, deserr::Deserr,
)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[deserr(rename_all = camelCase, deny_unknown_fields)]
pub enum PromptFallbackStrategy {
    Fallback,
    Skip,
    #[default]
    Error,
}

#[cfg(test)]
mod test {
    use super::Prompt;
    use crate::error::FaultSource;
    use crate::prompt::error::{NewPromptError, NewPromptErrorKind};

    #[test]
    fn default_template() {
        // does not panic
        Prompt::default();
    }

    #[test]
    fn empty_template() {
        Prompt::new("".into(), None, None).unwrap();
    }

    #[test]
    fn template_ok() {
        Prompt::new("{{doc.title}}: {{doc.overview}}".into(), None, None).unwrap();
    }

    #[test]
    fn template_syntax() {
        assert!(matches!(
            Prompt::new("{{doc.title: {{doc.overview}}".into(), None, None),
            Err(NewPromptError {
                kind: NewPromptErrorKind::CannotParseTemplate(_),
                fault: FaultSource::User
            })
        ));
    }

    #[test]
    fn template_missing_doc() {
        assert!(matches!(
            Prompt::new("{{title}}: {{overview}}".into(), None, None),
            Err(NewPromptError {
                kind: NewPromptErrorKind::InvalidFieldsInTemplate(_),
                fault: FaultSource::User
            })
        ));
    }

    #[test]
    fn template_nested_doc() {
        Prompt::new("{{doc.actor.firstName}}: {{doc.actor.lastName}}".into(), None, None).unwrap();
    }

    #[test]
    fn template_fields() {
        Prompt::new("{% for field in fields %}{{field}}{% endfor %}".into(), None, None).unwrap();
    }

    #[test]
    fn template_fields_ok() {
        Prompt::new(
            "{% for field in fields %}{{field.name}}: {{field.value}}{% endfor %}".into(),
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn template_fields_invalid() {
        assert!(matches!(
            // intentionally garbled field
            Prompt::new("{% for field in fields %}{{field.vaelu}} {% endfor %}".into(), None, None),
            Err(NewPromptError {
                kind: NewPromptErrorKind::InvalidFieldsInTemplate(_),
                fault: FaultSource::User
            })
        ));
    }
}
