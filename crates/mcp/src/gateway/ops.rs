use super::*;

impl Gateway {
    pub(super) fn gateway_call(
        &self,
        params: &Map<String, Value>,
        server_param: Option<&str>,
        tool: &str,
        cancel: Option<&CancelToken>,
    ) -> Outcome {
        let args = parse_args(params.get("args"))?;
        let (connection, found) = self.find_tool(tool, server_param)?;
        self.connect(&connection)?;
        let original = tool_name(&found).to_string();
        let result = connection
            .call_tool(&original, &args, cancel)
            .map_err(GatewayError::other)?;
        let converted = convert_mcp_result(
            connection.name(),
            &original,
            &result,
            connection.has_listed_resources(),
            &save_to_temp_file,
        );
        let mut details = json!({ "mode": "call", "server": connection.name(), "tool": original });
        if let Some(structured) = result.get("structuredContent") {
            details["structuredContent"] = structured.clone();
        }
        if converted["isError"] == true {
            details["error"] = json!("tool_error");
        }
        Ok(json!({ "content": converted["content"], "details": details }))
    }

    pub(super) fn gateway_connect(&self, name: &str) -> Outcome {
        let connection = self.connection_named(name)?;
        if connection.state() == ServerState::Connected {
            connection.reconnect().map_err(GatewayError::other)?;
        } else {
            self.connect(&connection)?;
        }
        let entry = self.entry_named(name)?;
        let count = visible_tools(&entry, Some(&connection)).len();
        Ok(text(
            format!("Connected to {name} ({count} tools)."),
            json!({ "mode": "connect", "server": name }),
        ))
    }

    pub(super) fn gateway_describe(&self, name: &str, server_param: Option<&str>) -> Outcome {
        let (connection, tool) = self.find_tool(name, server_param)?;
        let server = connection.name();
        let original = tool_name(&tool);
        let description = tool["description"].as_str().unwrap_or_default();
        let mut described = json!({
            "name": display_name(server, original),
            "originalName": original,
            "description": description,
        });
        if let Some(schema) = tool.get("inputSchema") {
            described["inputSchema"] = schema.clone();
        }
        if let Some(schema) = tool.get("outputSchema") {
            described["outputSchema"] = schema.clone();
        }
        Ok(text(
            format!(
                "{}\nServer: {server}\n\n{description}\n\nParameters (JSON schema):\n{}",
                display_name(server, original),
                pretty(tool.get("inputSchema"))
            ),
            json!({ "mode": "describe", "tool": described, "server": server }),
        ))
    }

    pub(super) fn gateway_search(
        &self,
        params: &Map<String, Value>,
        search: &Value,
        server_param: Option<&str>,
    ) -> Outcome {
        let words: Vec<String> = search
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let mut matches: Vec<(u32, String, String, String)> = Vec::new();
        for entry in self.enabled_servers(server_param)? {
            let Ok(connection) = self.connected(&entry.name) else {
                continue;
            };
            for tool in visible_tools(&entry, Some(&connection)) {
                let score = if words.is_empty() {
                    1
                } else {
                    search_score(&words, &entry, &tool)
                };
                if score > 0 {
                    matches.push((
                        score,
                        entry.name.clone(),
                        display_name(&entry.name, tool_name(&tool)),
                        first_line(tool["description"].as_str().unwrap_or_default()).to_string(),
                    ));
                }
            }
        }
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| jsrt::collate::locale_compare(&a.2, &b.2)));
        let limit = params
            .get("limit")
            .and_then(Value::as_f64)
            .filter(|l| *l > 0.0)
            .map_or(20, |l| l.trunc() as usize);
        let limited: Vec<&(u32, String, String, String)> = matches.iter().take(limit).collect();
        let mut lines = vec![format!("Found {} tools", matches.len())];
        lines.extend(limited.iter().map(|m| format!("{}: {}", m.2, m.3)));
        let listed: Vec<Value> = limited
            .iter()
            .map(|m| json!({ "server": m.1, "tool": m.2, "score": m.0, "description": m.3 }))
            .collect();
        Ok(text(
            lines.join("\n"),
            json!({ "mode": "search", "matches": listed, "count": matches.len() }),
        ))
    }

    pub(super) fn gateway_read(&self, uri: &str, server_param: Option<&str>, cancel: Option<&CancelToken>) -> Outcome {
        let Some(name) = server_param else {
            return Err(GatewayError::new(
                "invalid_args",
                "Reading a resource needs the server name".into(),
            ));
        };
        self.entry_named(name)?;
        let connection = self.connected(name)?;
        let result = connection.read_resource(uri, cancel).map_err(GatewayError::other)?;
        let blocks: Vec<Value> = result["contents"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|resource| json!({ "type": "resource", "resource": resource }))
            .collect();
        let content = to_model_content(name, &blocks, false, &save_to_temp_file);
        Ok(json!({ "content": content, "details": { "mode": "read", "server": name, "uri": uri } }))
    }

    pub(super) fn gateway_resources(
        &self,
        resources: &Value,
        server_param: Option<&str>,
        cancel: Option<&CancelToken>,
    ) -> Outcome {
        let only = resources.as_str().or(server_param);
        let (mut found, mut templates, mut errors) = (Vec::new(), Vec::new(), Vec::new());
        for entry in self.enabled_servers(only)? {
            let listing = self.connected(&entry.name).and_then(|connection| {
                let resources = connection.all_resources(cancel).map_err(GatewayError::other)?;
                let templates = connection.all_resource_templates(cancel).map_err(GatewayError::other)?;
                Ok((resources, templates))
            });
            match listing {
                Ok((resources, list)) => {
                    found.extend(
                        resources
                            .iter()
                            .filter(|r| !is_mcp_app_resource(r))
                            .map(|r| listed(&entry.name, r)),
                    );
                    templates.extend(
                        list.iter()
                            .filter(|t| !is_mcp_app_resource(t))
                            .map(|t| listed(&entry.name, t)),
                    );
                }
                Err(error) => errors.push(json!({ "server": entry.name, "error": error.message })),
            }
        }
        let mut details = json!({ "mode": "resources", "resources": found, "resourceTemplates": templates });
        if !errors.is_empty() {
            details["errors"] = json!(errors);
        }
        Ok(text(
            format!("{} resources, {} resource templates", found.len(), templates.len()),
            details,
        ))
    }

    pub(super) fn gateway_list_server(&self, name: &str) -> Outcome {
        let entry = self.entry_named(name)?;
        let connection = self.connected(name)?;
        let tools: Vec<String> = visible_tools(&entry, Some(&connection))
            .iter()
            .map(|t| display_name(name, tool_name(t)))
            .collect();
        Ok(text(
            format!("{name} ({} tools)\n{}", tools.len(), tools.join("\n")),
            json!({ "mode": "list", "server": name, "tools": tools, "count": tools.len() }),
        ))
    }

    pub(super) fn action_logout(&self, name: &str) -> Result<String, GatewayError> {
        let connection = self.connection_named(name)?;
        let removed = match (connection.oauth_url(), &self.options.env.credentials) {
            (Some(url), Some(credentials)) => credentials
                .remove(name, &url)
                .map_err(|e| GatewayError::other(e.to_string()))?,
            _ => false,
        };
        connection.sign_out();
        Ok(if removed {
            format!("Signed out of \"{name}\".")
        } else {
            format!("No stored credentials for \"{name}\".")
        })
    }

    pub(super) fn action_set_enabled(
        &self,
        entry: &ServerEntry,
        name: &str,
        enabled: bool,
    ) -> Result<String, GatewayError> {
        if self.options.servers.is_none() {
            crate::edit::update_server_enabled(Path::new(&entry.source), name, enabled).map_err(GatewayError::other)?;
        }
        if !enabled {
            self.disconnect(name);
        }
        if let Some(server) = sync::lock(&self.state)
            .servers
            .iter_mut()
            .find(|s| s.entry.name == name)
        {
            if enabled {
                server.entry.config.shift_remove("enabled");
            } else {
                server.entry.config.insert("enabled".into(), json!(false));
            }
        }
        let verb = if enabled { "Enabled" } else { "Disabled" };
        let place = match self.options.servers {
            None => format!(" in {}", entry.source),
            Some(_) => " for this session".into(),
        };
        Ok(format!("{verb} \"{name}\"{place}."))
    }

    pub(super) fn action_list_tools(&self, entry: &ServerEntry, name: &str) -> Result<String, GatewayError> {
        let connection = self.connected(name)?;
        let tools = visible_tools(entry, Some(&connection));
        let mut lines = vec![format!("{name}: {} tools", tools.len())];
        lines.extend(tools.iter().map(|tool| {
            format!(
                "  {}: {}",
                display_name(name, tool_name(tool)),
                first_line(tool["description"].as_str().unwrap_or_default())
            )
        }));
        Ok(lines.join("\n"))
    }
}
