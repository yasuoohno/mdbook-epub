use std::iter;
use std::io::{Read, Write};
use std::fmt::{self, Debug, Formatter};
use std::fs::File;

use mdbook::renderer::RenderContext;
use mdbook::book::{BookItem, Chapter};
use epub_builder::{EpubBuilder, EpubContent, ZipLibrary};
use failure::{Error, ResultExt};
use pulldown_cmark::{html, Parser, Options, Event, CowStr, Tag};
use handlebars::{Handlebars, RenderError};

use crate::config::Config;
use crate::resources::{self, Asset};
use crate::utils::ResultExt as SyncResultExt;
use crate::DEFAULT_CSS;

/// The actual EPUB book renderer.
pub struct Generator<'a> {
    ctx: &'a RenderContext,
    builder: EpubBuilder<ZipLibrary>,
    config: Config,
    hbs: Handlebars<'a>,
}

impl<'a> Generator<'a> {
    pub fn new(ctx: &'a RenderContext) -> Result<Generator<'a>, Error> {
        let builder = EpubBuilder::new(ZipLibrary::new().sync()?).sync()?;
        let config = Config::from_render_context(ctx)?;

        let mut hbs = Handlebars::new();
        hbs.register_template_string("index", config.template()?)
            .context("Couldn't parse the template")?;

        Ok(Generator {
            builder,
            ctx,
            config,
            hbs,
        })
    }

    fn populate_metadata(&mut self) -> Result<(), Error> {
        self.builder.metadata("generator", "mdbook-epub").sync()?;

        if let Some(title) = self.ctx.config.book.title.clone() {
            self.builder.metadata("title", title).sync()?;
        } else {
            warn!("No `title` attribute found yet all EPUB documents should have a title");
        }

        if let Some(desc) = self.ctx.config.book.description.clone() {
            self.builder.metadata("description", desc).sync()?;
        }

        if !self.ctx.config.book.authors.is_empty() {
            self.builder
                .metadata("author", self.ctx.config.book.authors.join(", "))
                .sync()?;
        }

        self.builder
            .metadata("generator", env!("CARGO_PKG_NAME"))
            .sync()?;
        self.builder.metadata("lang", "en").sync()?;

        Ok(())
    }

    pub fn generate<W: Write>(mut self, writer: W) -> Result<(), Error> {
        info!("Generating the EPUB book");

        self.populate_metadata()?;
        self.generate_chapters()?;

        self.add_cover_image()?;
        self.embed_stylesheets()?;
        self.additional_assets()?;
        self.additional_resources()?;
        self.builder.generate(writer).sync()?;

        Ok(())
    }

    fn generate_chapters(&mut self) -> Result<(), Error> {
        debug!("Rendering Chapters");

        for item in &self.ctx.book.sections {
            if let BookItem::Chapter(ref ch) = *item {
                debug!("Adding chapter \"{}\"", ch);
                self.add_chapter(ch)?;
            }
        }

        Ok(())
    }

    fn add_chapter(&mut self, ch: &Chapter) -> Result<(), Error> {
        let rendered = self.render_chapter(ch)?;

        let content_path = ch.path.as_ref()
            .ok_or_else(|| failure::err_msg(format!("No content file is found by a path = {:?}", ch.path)))?;
        trace!("add a chapter {:?} by a path = {:?}", &ch.name, content_path);
        let path = content_path.with_extension("html").display().to_string();

        let title = if self.config.no_section_label {
            ch.name.clone()
        } else if let Some(ref section_number) = ch.number {
            format!{"{} {}", section_number, ch.name}
        } else {
            ch.name.clone()
        };

        let mut content = EpubContent::new(path, rendered.as_bytes()).title(title);

        let level = ch.number.as_ref().map(|n| n.len() as i32 - 1).unwrap_or(0);
        content = content.level(level);

        self.builder.add_content(content).sync()?;

        // second pass to actually add the sub-chapters
        for sub_item in &ch.sub_items {
            if let BookItem::Chapter(ref sub_ch) = *sub_item {
                trace!("add sub-item = {:?}", sub_ch.name);
                self.add_chapter(sub_ch)?;
            }
        }

        Ok(())
    }

    pub fn new_cmark_parser(text: &str) -> Parser<'_> {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_FOOTNOTES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        Parser::new_ext(text, opts)
    }

    /// Render the chapter into its fully formed HTML representation.
    fn render_chapter(&self, ch: &Chapter) -> Result<String, RenderError> {
        let mut body = String::new();
        let p = Generator::new_cmark_parser(&ch.content);
        let mut converter = EventQuoteConverter::new(self.config.curly_quotes);
        let events = p.map(|event| converter.convert(event));

        html::push_html(&mut body, events);

        let css_path = ch.path.as_ref()
            .ok_or_else(|| RenderError::new(format!("No CSS found by a path =  = {:?}", ch.path)))?;

        let stylesheet_path = css_path
            .parent()
            .expect("All chapters have a parent")
            .components()
            .map(|_| "..")
            .chain(iter::once("stylesheet.css"))
            .collect::<Vec<_>>()
            .join("/");

        let ctx = json!({ "title": ch.name, "body": body, "stylesheet": stylesheet_path });

        self.hbs.render("index", &ctx)
    }

    /// Generate the stylesheet and add it to the document.
    fn embed_stylesheets(&mut self) -> Result<(), Error> {
        debug!("Embedding stylesheets");

        let stylesheet = self
            .generate_stylesheet()
            .context("Unable to generate stylesheet")?;
        self.builder.stylesheet(stylesheet.as_slice()).sync()?;

        Ok(())
    }

    fn additional_assets(&mut self) -> Result<(), Error> {
        debug!("Embedding additional assets");

        let assets = resources::find(self.ctx)
            .context("Inspecting the book for additional assets failed")?;

        for asset in assets {
            debug!("Embedding {}", asset.filename.display());
            self.load_asset(&asset)
                .with_context(|_| format!("Couldn't load {}", asset.filename.display()))?;
        }

        Ok(())
    }

    fn additional_resources(&mut self) -> Result<(), Error> {
        debug!("Embedding additional resources");

        for path in self.config.additional_resources.iter() {
            debug!("Embedding {:?}", path);

            let name = path.file_name().unwrap_or_else(|| panic!("Can't determine file name of: {:?}", &path));
            let full_path = path.canonicalize()?;
            let mt = mime_guess::from_path(&full_path).first_or_octet_stream();

            let content = File::open(&full_path).context("Unable to open asset").unwrap();

            self.builder.add_resource(&name, content, mt.to_string()).sync()?;
        }

        Ok(())
    }

    fn add_cover_image(&mut self) -> Result<(), Error> {
        debug!("Adding cover image");

        if let Some(ref path) = self.config.cover_image {
            let name = path.file_name().expect("Can't provide file name.");
            let full_path = path.canonicalize()?;
            let mt = mime_guess::from_path(&full_path).first_or_octet_stream();

            let content = File::open(&full_path).context("Unable to open asset")?;

            self.builder.add_cover_image(&name, content, mt.to_string()).sync()?;
        }

        Ok(())
    }

    fn load_asset(&mut self, asset: &Asset) -> Result<(), Error> {
        let content = File::open(&asset.location_on_disk).context("Unable to open asset")?;

        let mt = asset.mimetype.to_string();

        self.builder
            .add_resource(&asset.filename, content, mt)
            .sync()?;

        Ok(())
    }

    /// Concatenate all provided stylesheets into one long stylesheet.
    fn generate_stylesheet(&self) -> Result<Vec<u8>, Error> {
        let mut stylesheet = Vec::new();

        if self.config.use_default_css {
            stylesheet.extend(DEFAULT_CSS.as_bytes());
        }

        for additional_css in &self.config.additional_css {
            let mut f = File::open(&additional_css)
                .with_context(|_| format!("Unable to open {}", additional_css.display()))?;
            f.read_to_end(&mut stylesheet)
                .context("Error reading stylesheet")?;
        }

        Ok(stylesheet)
    }
}

impl<'a> Debug for Generator<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("Generator")
            .field("ctx", &self.ctx)
            .field("builder", &self.builder)
            .field("config", &self.config)
            .finish()
    }
}

/// From `mdbook/src/utils/mod.rs`, where this is a private struct.
struct EventQuoteConverter {
    enabled: bool,
    convert_text: bool,
}

impl EventQuoteConverter {
    fn new(enabled: bool) -> Self {
        EventQuoteConverter {
            enabled,
            convert_text: true,
        }
    }

    fn convert<'a>(&mut self, event: Event<'a>) -> Event<'a> {
        if !self.enabled {
            return event;
        }

        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                self.convert_text = false;
                event
            }
            Event::End(Tag::CodeBlock(_)) => {
                self.convert_text = true;
                event
            }
            Event::Text(ref text) if self.convert_text => {
                Event::Text(CowStr::from(convert_quotes_to_curly(text)))
            }
            _ => event,
        }
    }
}

fn convert_quotes_to_curly(original_text: &str) -> String {
    // We'll consider the start to be "whitespace".
    let mut preceded_by_whitespace = true;

    original_text
        .chars()
        .map(|original_char| {
            let converted_char = match original_char {
                '\'' => {
                    if preceded_by_whitespace {
                        '‘'
                    } else {
                        '’'
                    }
                }
                '"' => {
                    if preceded_by_whitespace {
                        '“'
                    } else {
                        '”'
                    }
                }
                _ => original_char,
            };

            preceded_by_whitespace = original_char.is_whitespace();

            converted_char
        })
        .collect()
}

