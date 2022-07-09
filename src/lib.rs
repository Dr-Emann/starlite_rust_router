use pyo3::prelude::*;
use pyo3::types::{PyList, PyMapping, PySequence, PyType};

use ahash::AHashSet as HashSet;
use ahash::{AHashMap as HashMap, AHashSet};
use pyo3::exceptions::PyTypeError;
use std::collections::HashMap as StdHashMap;

type ASGIApp = PyAny;

mod exceptions {
    pyo3::import_exception!(starlite.exceptions, ImproperlyConfiguredException);
    pyo3::import_exception!(starlite.exceptions, MethodNotAllowedException);
    pyo3::import_exception!(starlite.exceptions, NotFoundException);
}

#[pyclass]
#[derive(Debug)]
struct RouteMap {
    app: StarliteApp,
    route_types: RouteTypes,
    path_param_parser: Py<PyAny>,
    param_routes: Node,
    plain_routes: HashMap<String, Leaf>,
}

#[derive(Debug, Default)]
struct Node {
    children: HashMap<String, Node>,
    placeholder_child: Option<Box<Node>>,
    leaf: Option<Leaf>,
}

#[derive(Debug)]
struct Leaf {
    is_asgi: bool,
    static_path: Option<String>,
    path_parameters: Py<PyAny>,
    asgi_handlers: HashMap<HandlerType, Py<ASGIApp>>,
}

impl Leaf {
    fn new(params: Py<PyAny>) -> Self {
        Self {
            path_parameters: params,
            asgi_handlers: Default::default(),
            is_asgi: false,
            static_path: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum HandlerType {
    Asgi,
    Websocket,
    // HTTP methods taken from starlite.types.Method
    HttpGet,
    HttpPost,
    HttpDelete,
    HttpPatch,
    HttpPut,
    HttpHead,
    HttpOther(String),
}

impl HandlerType {
    fn from_http_method(method: &str) -> Self {
        match method {
            "GET" => Self::HttpGet,
            "POST" => Self::HttpPost,
            "DELETE" => Self::HttpDelete,
            "PATCH" => Self::HttpPatch,
            "PUT" => Self::HttpPut,
            "HEAD" => Self::HttpHead,
            _ => Self::HttpOther(String::from(method)),
        }
    }
}

fn split_path(path: &str) -> impl Iterator<Item = &'_ str> {
    path.split('/').filter(|s| !s.is_empty())
}

fn build_param_set<'a>(
    path_parameters: &[&'a PyAny],
    param_strings: &mut HashSet<&'a str>,
) -> PyResult<()> {
    param_strings.clear();
    param_strings.reserve(path_parameters.len());
    for &path_param in path_parameters {
        let full_name: &str = path_param
            .get_item(pyo3::intern!(path_param.py(), "full"))?
            .extract()?;
        param_strings.insert(full_name);
    }
    Ok(())
}

impl RouteMap {
    fn add_routes_(&mut self, items: &PySequence) -> PyResult<()> {
        let p = items.py();
        let mut param_strings = HashSet::new();
        for route in items.iter()? {
            let route: &PyAny = route?;
            let base: BaseRoute = route.extract()?;
            let path = base.path;
            let path_parameters: Vec<&PyAny> = base.path_parameters.extract()?;

            let in_static = self.app.path_in_static(p, path)?;
            let leaf: &mut Leaf = if !path_parameters.is_empty() || in_static {
                build_param_set(&path_parameters, &mut param_strings)?;

                let mut node = &mut self.param_routes;
                for s in split_path(path) {
                    let is_placeholder = s.starts_with('{')
                        && s.ends_with('}')
                        && param_strings.contains(&s[1..s.len() - 1]);

                    node = if is_placeholder {
                        node.placeholder_child.get_or_insert_with(Default::default)
                    } else {
                        node.children
                            .entry(String::from(s))
                            .or_insert_with(Default::default)
                    };
                }
                // Found where the leaf should be, get it, or add a new one
                node.leaf
                    .get_or_insert_with(|| Leaf::new(base.path_parameters.into()))
            } else {
                self.plain_routes
                    .entry(String::from(path))
                    .or_insert_with(|| Leaf::new(base.path_parameters.into()))
            };
            if leaf.path_parameters.as_ref(p).ne(base.path_parameters)? {
                return Err(exceptions::ImproperlyConfiguredException::new_err(
                    "Routes with conflicting path parameters",
                ));
            }
            if in_static {
                leaf.is_asgi = true;
                leaf.static_path = Some(String::from(path));
            }

            let route_types = &self.route_types;
            if route.is_instance(route_types.http.as_ref(p))? {
                let http_route: HttpRoute<'_> = route.extract()?;
                for (method, (handler, _)) in http_route.route_handler_map {
                    leaf.asgi_handlers.insert(
                        HandlerType::from_http_method(method),
                        self.app.build_route(route, handler)?,
                    );
                }
            } else if route.is_instance(route_types.websocket.as_ref(p))? {
                let SingleHandlerRoute { handler } = route.extract()?;
                leaf.asgi_handlers.insert(
                    HandlerType::Websocket,
                    self.app.build_route(route, handler)?,
                );
            } else if route.is_instance(route_types.asgi.as_ref(p))? {
                let SingleHandlerRoute { handler } = route.extract()?;
                // TODO: Can do better than a a string
                leaf.asgi_handlers
                    .insert(HandlerType::Asgi, self.app.build_route(route, handler)?);
                leaf.is_asgi = true;
            } else {
                return Err(PyTypeError::new_err("Unknown route type"));
            }
        }
        Ok(())
    }

    fn resolve_route_(&self, scope: &PyMapping) -> PyResult<Py<PyAny>> {
        let py = scope.py();
        let path: &str = scope.get_item(pyo3::intern!(py, "path"))?.extract()?;
        let mut path = path.strip_suffix(|ch| ch == '/').unwrap_or(path);
        if path.is_empty() {
            path = "/";
        }
        let (leaf, params) = match self.plain_routes.get(path) {
            Some(leaf) => (leaf, PyList::empty(py)),
            None => self.find_route(path, scope)?,
        };
        scope.set_item(
            pyo3::intern!(py, "path_params"),
            self.parse_path_params(leaf.path_parameters.as_ref(py), params)?,
        )?;

        let handler: Option<&Py<ASGIApp>> = if leaf.is_asgi {
            leaf.asgi_handlers.get(&HandlerType::Asgi)
        } else {
            let scope_type: &str = scope.get_item(pyo3::intern!(py, "type"))?.extract()?;
            if scope_type == "http" {
                let scope_method: &str = scope.get_item(pyo3::intern!(py, "method"))?.extract()?;
                let handler = leaf
                    .asgi_handlers
                    .get(&HandlerType::from_http_method(scope_method));
                if handler.is_none() {
                    return Err(exceptions::MethodNotAllowedException::new_err(()));
                }
                handler
            } else {
                leaf.asgi_handlers.get(&HandlerType::Websocket)
            }
        };
        let handler: Py<ASGIApp> = handler
            .ok_or_else(|| exceptions::NotFoundException::new_err(()))?
            .clone_ref(py);
        Ok(handler)
    }

    fn find_route<'a>(&'a self, path: &str, scope: &'a PyMapping) -> PyResult<(&Leaf, &PyList)> {
        let py = scope.py();
        let key_path = pyo3::intern!(py, "path");
        let mut params = Vec::new();
        let mut node = &self.param_routes;
        for component in split_path(path) {
            if let Some(child) = node.children.get(component) {
                node = child;
                continue;
            }
            if let Some(child) = &node.placeholder_child {
                node = child;
                params.push(component);
                continue;
            }
            let static_path = node
                .leaf
                .as_ref()
                .and_then(|leaf| leaf.static_path.as_deref());
            if let Some(static_path) = static_path {
                if static_path != "/" {
                    let old_scope_path: &str = scope.get_item(key_path)?.extract()?;
                    let new_scope_path = old_scope_path.replace(static_path, "");
                    scope.set_item(key_path, new_scope_path)?;
                }
                continue;
            }

            return Err(exceptions::NotFoundException::new_err(()));
        }
        let leaf = match &node.leaf {
            Some(leaf) => leaf,
            None => return Err(exceptions::NotFoundException::new_err(())),
        };
        let list = PyList::new(py, params);
        Ok((leaf, list))
    }

    fn parse_path_params(&self, params: &PyAny, values: &PyList) -> PyResult<Py<PyAny>> {
        self.path_param_parser.call1(params.py(), (params, values))
    }
}

#[derive(Debug, FromPyObject)]
struct BaseRoute<'a> {
    path: &'a str,
    path_parameters: &'a PyAny,
}

#[derive(Debug, FromPyObject)]
struct HttpRoute<'a> {
    route_handler_map: StdHashMap<&'a str, (&'a PyAny, &'a PyAny)>,
}

#[derive(Debug, FromPyObject)]
struct SingleHandlerRoute<'a> {
    #[pyo3(attribute("route_handler"))]
    handler: &'a PyAny,
}

#[derive(Debug, FromPyObject)]
struct StarliteApp {
    static_paths: Py<PyAny>,
    build_route_middleware_stack: Py<PyAny>,
}

impl StarliteApp {
    fn path_in_static(&self, py: Python<'_>, path: &str) -> PyResult<bool> {
        self.static_paths.as_ref(py).contains(path)
    }

    fn build_route(&self, route: &PyAny, handler: &PyAny) -> PyResult<Py<PyAny>> {
        let py = route.py();
        self.build_route_middleware_stack
            .call1(py, (route, handler))
    }
}

#[derive(Debug, Clone)]
struct RouteTypes {
    http: Py<PyType>,
    websocket: Py<PyType>,
    asgi: Py<PyType>,
}

#[pymethods]
impl RouteMap {
    #[new]
    fn new(py: Python<'_>, app: StarliteApp) -> PyResult<Self> {
        let module = py.import("starlite.routes")?;
        let extract_type = |name: &str| -> PyResult<Py<PyType>> {
            let any: &PyAny = module.getattr(name)?;
            Ok(any.downcast::<PyType>()?.into())
        };
        let route_types = RouteTypes {
            http: extract_type("HTTPRoute")?,
            websocket: extract_type("WebSocketRoute")?,
            asgi: extract_type("ASGIRoute")?,
        };

        let module = py.import("starlite.parsers")?;
        let path_param_parser = module.getattr("parse_path_params")?.into();
        Ok(Self {
            app,
            route_types,
            path_param_parser,
            param_routes: Node::default(),
            plain_routes: HashMap::default(),
        })
    }

    fn __repr__(&self) -> String {
        format!("{:#?}", self)
    }

    /// Add an item
    #[pyo3(text_signature = "(routes)")]
    fn add_routes(&mut self, routes: &PySequence) -> PyResult<()> {
        self.add_routes_(routes)
    }

    #[pyo3(text_signature = "(scope)")]
    fn resolve_route(&self, scope: &PyMapping) -> PyResult<Py<PyAny>> {
        self.resolve_route_(scope)
    }
}

/// A Python module implemented in Rust.
#[pymodule]
fn starlite_router(_p: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<RouteMap>()?;
    Ok(())
}
