//! Unified HTTP Parser Module
//!
//! This module provides zero-copy HTTP parsing implementation for high-performance request processing.
//! All parsing functions use direct buffer references for maximum efficiency.
//! Handles both HTTP request and response parsing in a unified interface.

use anyhow::Result;
use tracing::debug;

/// HTTP Connection header preference
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionType {
    /// Explicit Connection: keep-alive
    KeepAlive,
    /// Explicit Connection: close  
    Close,
    /// No Connection header - use HTTP version default
    Default,
}

/// HTTP version detected from request line
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HttpVersion {
    /// HTTP/1.0 - default connection: close
    Http10,
    /// HTTP/1.1 - default connection: keep-alive
    Http11,
}



/// Zero-copy HTTP parser for extracting routing information from requests
/// All parsing functions use zero-copy operations with direct buffer references
pub struct UnifiedHttpParser;

impl UnifiedHttpParser {
    /// Extract complete HTTP routing information with zero-copy parsing
    /// Single-pass algorithm - parses method, host, path, headers in one scan
    #[inline(always)]
    pub fn extract_routing_info(
        request_data: &[u8],
    ) -> Result<HttpRequestInfo<'_>> {
        debug!("UNIFIED_PARSER: Processing HTTP request - content length: {}", request_data.len());
        
        // Single-pass state machine parser
        let mut pos = 0;
        let mut path: Option<&str> = None;
        let mut host: Option<&str> = None;
        let mut raw_connection_type = ConnectionType::Default;
        let mut content_length: Option<usize> = None;
        let mut http_version = HttpVersion::Http11;
        
        // Parse first line (method, path, version)
        let mut line_end = pos;
        while line_end < request_data.len() && 
              request_data[line_end] != b'\r' && 
              request_data[line_end] != b'\n' {
            line_end += 1;
        }
        
        if line_end > pos {
            if let Ok(first_line) = std::str::from_utf8(&request_data[pos..line_end]) {
                let mut parts = first_line.split_whitespace();
                parts.next(); // Skip method
                path = parts.next();
                
                // Extract HTTP version from end of line
                if line_end >= 8 && &request_data[line_end-8..line_end] == b"HTTP/1.0" {
                    http_version = HttpVersion::Http10;
                } else if line_end >= 8 && &request_data[line_end-8..line_end] == b"HTTP/1.1" {
                    http_version = HttpVersion::Http11;
                }
            }
        }
        
        // Skip to headers (past CRLF)
        pos = line_end;
        if pos < request_data.len() && request_data[pos] == b'\r' { pos += 1; }
        if pos < request_data.len() && request_data[pos] == b'\n' { pos += 1; }
        
        // Parse headers in single pass
        while pos < request_data.len() {
            // Check for end of headers
            if pos + 1 < request_data.len() && &request_data[pos..pos+2] == b"\r\n" {
                break;
            }
            
            let line_start = pos;
            let mut line_end = pos;
            while line_end < request_data.len() && 
                  request_data[line_end] != b'\r' && 
                  request_data[line_end] != b'\n' {
                line_end += 1;
            }
            
            if line_end > line_start {
                let line = &request_data[line_start..line_end];
                
                // Fast header matching using first few bytes
                if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"Host:") {
                    let mut value_start = 5;
                    while value_start < line.len() && line[value_start] == b' ' {
                        value_start += 1;
                    }
                    
                    let mut value_end = line.len();
                    while value_end > value_start && 
                          (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t') {
                        value_end -= 1;
                    }
                    
                    // Find port separator and truncate for Gateway API compliance
                    let mut normalized_end = value_end;
                    for i in value_start..value_end {
                        if line[i] == b':' {
                            normalized_end = i;
                            break;
                        }
                    }
                    
                    if let Ok(host_str) = std::str::from_utf8(&line[value_start..normalized_end]) {
                        host = Some(host_str);
                    }
                    
                } else if line.len() >= 11 && line[..11].eq_ignore_ascii_case(b"Connection:") {
                    let mut value_start = 11;
                    while value_start < line.len() && line[value_start] == b' ' {
                        value_start += 1;
                    }
                    
                    let mut value_end = line.len();
                    while value_end > value_start && 
                          (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t') {
                        value_end -= 1;
                    }
                    
                    if let Ok(value) = std::str::from_utf8(&line[value_start..value_end]) {
                        if value.eq_ignore_ascii_case("close") {
                            raw_connection_type = ConnectionType::Close;
                        } else if value.eq_ignore_ascii_case("keep-alive") {
                            raw_connection_type = ConnectionType::KeepAlive;
                        }
                    }
                    
                } else if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"Content-Length:") {
                    let mut value_start = 15;
                    while value_start < line.len() && line[value_start] == b' ' {
                        value_start += 1;
                    }
                    
                    let mut value_end = line.len();
                    while value_end > value_start && 
                          (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t') {
                        value_end -= 1;
                    }
                    
                    if let Ok(value_str) = std::str::from_utf8(&line[value_start..value_end]) {
                        content_length = value_str.parse().ok();
                    }
                }
            }
            
            // Move to next line
            pos = line_end;
            if pos < request_data.len() && request_data[pos] == b'\r' { pos += 1; }
            if pos < request_data.len() && request_data[pos] == b'\n' { pos += 1; }
        }
        
        // Apply HTTP version-aware connection defaults
        let connection_type = match raw_connection_type {
            ConnectionType::Close => ConnectionType::Close,
            ConnectionType::KeepAlive => ConnectionType::KeepAlive,
            ConnectionType::Default => {
                match http_version {
                    HttpVersion::Http10 => ConnectionType::Close,
                    HttpVersion::Http11 => ConnectionType::KeepAlive,
                }
            }
        };
        
        Ok(HttpRequestInfo {
            host: host.unwrap_or("localhost"),
            path: path.unwrap_or("/"),
            connection_type,
            content_length,
        })
    }
    
    /// Parse backend response to determine if connection should be pooled
    /// Checks for Connection: close header which indicates connection should not be reused
    #[inline(always)]
    pub fn should_pool_backend_connection(response_data: &[u8]) -> bool {
        // Quick check for HTTP response
        if response_data.len() < 12 || !response_data.starts_with(b"HTTP/") {
            return false;
        }
        
        // Find HTTP version in first line
        let mut pos = 0;
        let mut http_version = HttpVersion::Http11;
        
        // Parse first line to get HTTP version
        let mut line_end = pos;
        while line_end < response_data.len() && 
              response_data[line_end] != b'\r' && 
              response_data[line_end] != b'\n' {
            line_end += 1;
        }
        
        // Check HTTP version
        if line_end >= 8 {
            if &response_data[5..8] == b"1.0" {
                http_version = HttpVersion::Http10;
            } else if &response_data[5..8] == b"1.1" {
                http_version = HttpVersion::Http11;
            }
        }
        
        // Skip to headers
        pos = line_end;
        if pos < response_data.len() && response_data[pos] == b'\r' { pos += 1; }
        if pos < response_data.len() && response_data[pos] == b'\n' { pos += 1; }
        
        // Parse headers looking for Connection header
        let mut found_connection_close = false;
        let mut found_connection_keepalive = false;
        
        while pos < response_data.len() {
            // Check for end of headers
            if pos + 1 < response_data.len() && &response_data[pos..pos+2] == b"\r\n" {
                break;
            }
            
            let line_start = pos;
            let mut line_end = pos;
            while line_end < response_data.len() && 
                  response_data[line_end] != b'\r' && 
                  response_data[line_end] != b'\n' {
                line_end += 1;
            }
            
            if line_end > line_start {
                let line = &response_data[line_start..line_end];
                
                // Check for Connection header
                if line.len() >= 10 && line[..10].eq_ignore_ascii_case(b"Connection") {
                    // Find the colon
                    let mut colon_pos = 10;
                    while colon_pos < line.len() && line[colon_pos] != b':' {
                        colon_pos += 1;
                    }
                    
                    if colon_pos < line.len() {
                        let mut value_start = colon_pos + 1;
                        while value_start < line.len() && 
                              (line[value_start] == b' ' || line[value_start] == b'\t') {
                            value_start += 1;
                        }
                        
                        if value_start < line.len() {
                            let value = &line[value_start..];
                            if value.len() >= 5 && value[..5].eq_ignore_ascii_case(b"close") {
                                found_connection_close = true;
                            } else if value.len() >= 10 && value[..10].eq_ignore_ascii_case(b"keep-alive") {
                                found_connection_keepalive = true;
                            }
                        }
                    }
                }
            }
            
            // Move to next line
            pos = line_end;
            if pos < response_data.len() && response_data[pos] == b'\r' { pos += 1; }
            if pos < response_data.len() && response_data[pos] == b'\n' { pos += 1; }
        }
        
        // Determine if we should pool based on what we found
        if found_connection_close {
            // Explicit close - don't pool
            false
        } else if found_connection_keepalive {
            // Explicit keep-alive - pool it
            true
        } else {
            // Use HTTP version default
            match http_version {
                HttpVersion::Http10 => false, // HTTP/1.0 defaults to close
                HttpVersion::Http11 => true,  // HTTP/1.1 defaults to keep-alive
            }
        }
    }
    
    
}


/// HTTP request information extracted by unified parser (zero-copy)
/// Contains all fields needed for routing decisions and execution
#[derive(Debug, Clone)]
pub struct HttpRequestInfo<'a> {
    pub host: &'a str,
    pub path: &'a str,
    pub connection_type: ConnectionType,
    pub content_length: Option<usize>,
}

