use anyhow::{anyhow, Result};
use std::str;

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

/// Parser type for universal header extraction
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParserType {
    /// Parse request headers (Host, Connection)
    Request,
    /// Parse response headers (Content-Length, Connection)
    Response,
}

/// Universal header information for both requests and responses
#[derive(Debug, Clone)]
pub struct HeaderInfo<'a> {
    /// Host header value (requests only)
    pub host: Option<&'a str>,
    /// Connection type for both requests and responses
    pub connection_type: ConnectionType,
    /// Content-Length header value (responses only)
    pub content_length: Option<usize>,
}

/// HTTP header parsing with zero allocations
/// Returns (method, path, host, connection) as string slices referencing the original buffer
/// Target: <300ns parsing time for typical requests (true single-pass optimization)
#[inline(always)]
pub fn parse_http_headers_fast(request: &[u8]) -> Result<(&str, &str, Option<&str>, ConnectionType)> {
    // Fast bounds check
    if request.len() < 16 {  // Minimum for "GET / HTTP/1.1\r\n\r\n"
        return Err(anyhow!("Request too short"));
    }

    // Single-pass parsing: extract method, path, and find host in one scan
    parse_http_single_pass(request)
}

/// Single-pass HTTP parsing that combines method, path, host, and connection extraction
/// Eliminates multiple scans through the request data for maximum performance
#[inline(always)]
fn parse_http_single_pass(request: &[u8]) -> Result<(&str, &str, Option<&str>, ConnectionType)> {
    let mut pos = 0;
    
    // Parse method (first token before space)
    let method_end = find_next_space(request, pos)
        .ok_or_else(|| anyhow!("Invalid request line: no space after method"))?;
    
    // SAFETY: HTTP method contains only ASCII chars
    let method = unsafe { std::str::from_utf8_unchecked(&request[0..method_end]) };
    pos = method_end + 1;
    
    // Parse path (second token before space)
    let path_end = find_next_space(request, pos)
        .ok_or_else(|| anyhow!("Invalid request line: no space after path"))?;
    
    // SAFETY: HTTP paths are percent-encoded ASCII
    let path = unsafe { std::str::from_utf8_unchecked(&request[pos..path_end]) };
    pos = path_end + 1;
    
    // Validate HTTP version is present (third token)
    let version_start = pos;
    let mut version_end = pos;
    while version_end < request.len() && request[version_end] != b'\r' {
        version_end += 1;
    }
    
    if version_end == version_start {
        return Err(anyhow!("Invalid request line: missing HTTP version"));
    }
    
    // Basic validation that it looks like HTTP version
    if version_end - version_start < 8 || !request[version_start..version_start + 5].eq_ignore_ascii_case(b"HTTP/") {
        return Err(anyhow!("Invalid request line: malformed HTTP version"));
    }
    
    // Find end of request line and start of headers
    pos = version_end;
    if pos < request.len() - 1 && request[pos] == b'\r' && request[pos + 1] == b'\n' {
        pos += 2; // Skip CRLF
    } else {
        return Err(anyhow!("Invalid request line: missing CRLF"));
    }
    
    // Single-pass header extraction for Host and Connection
    let headers_section = &request[pos..];
    let (host, connection) = extract_request_headers(headers_section)?;
    
    Ok((method, path, host, connection))
}


/// Universal high-performance header parser for both requests and responses
/// Single-pass extraction with zero function call overhead and optimal cache usage
/// Target: <300ns for both request and response parsing with maximum performance
#[inline(always)]
fn extract_headers<'a>(headers: &'a [u8], parser_type: ParserType) -> Result<HeaderInfo<'a>> {
    let mut pos = 0;
    let mut host_value: Option<&str> = None;
    let mut connection_type = ConnectionType::Default;
    let mut content_length: Option<usize> = None;
    
    // Early termination conditions based on parser type
    let needs_host = matches!(parser_type, ParserType::Request);
    let needs_content_length = matches!(parser_type, ParserType::Response);
    
    while pos < headers.len() {
        // Check for end of headers (\r\n\r\n) - early termination
        if pos + 3 < headers.len() && 
           headers[pos] == b'\r' && headers[pos + 1] == b'\n' && 
           headers[pos + 2] == b'\r' && headers[pos + 3] == b'\n' {
            break; // End of headers found
        }
        
        // Host header check (requests only) - requires 5 bytes minimum
        if needs_host && host_value.is_none() && pos + 4 < headers.len() &&
           (headers[pos] | 0x20) == b'h' &&
           (headers[pos + 1] | 0x20) == b'o' &&
           (headers[pos + 2] | 0x20) == b's' &&
           (headers[pos + 3] | 0x20) == b't' &&
           headers[pos + 4] == b':' {
            
            // Inline header value extraction for Host
            let mut value_start = pos + 5;
            
            // Skip leading whitespace after colon
            while value_start < headers.len() && 
                  (headers[value_start] == b' ' || headers[value_start] == b'\t') {
                value_start += 1;
            }
            
            // Find end of line
            let mut value_end = value_start;
            while value_end < headers.len() - 1 {
                if headers[value_end] == b'\r' && headers[value_end + 1] == b'\n' {
                    break;
                }
                value_end += 1;
            }
            
            // Trim trailing whitespace
            while value_end > value_start && 
                  (headers[value_end - 1] == b' ' || headers[value_end - 1] == b'\t') {
                value_end -= 1;
            }
            
            if value_start < value_end {
                // SAFETY: HTTP header values are ASCII-compatible
                host_value = Some(unsafe {
                    std::str::from_utf8_unchecked(&headers[value_start..value_end])
                });
            }
        }
        // Content-Length header check (responses only) - requires 15 bytes minimum
        else if needs_content_length && content_length.is_none() && pos + 14 < headers.len() &&
           (headers[pos] | 0x20) == b'c' &&
           (headers[pos + 1] | 0x20) == b'o' &&
           (headers[pos + 2] | 0x20) == b'n' &&
           (headers[pos + 3] | 0x20) == b't' &&
           (headers[pos + 4] | 0x20) == b'e' &&
           (headers[pos + 5] | 0x20) == b'n' &&
           (headers[pos + 6] | 0x20) == b't' &&
           headers[pos + 7] == b'-' &&
           (headers[pos + 8] | 0x20) == b'l' &&
           (headers[pos + 9] | 0x20) == b'e' &&
           (headers[pos + 10] | 0x20) == b'n' &&
           (headers[pos + 11] | 0x20) == b'g' &&
           (headers[pos + 12] | 0x20) == b't' &&
           (headers[pos + 13] | 0x20) == b'h' &&
           headers[pos + 14] == b':' {
            
            // Inline Content-Length value extraction
            let mut value_start = pos + 15;
            
            // Skip leading whitespace after colon
            while value_start < headers.len() && 
                  (headers[value_start] == b' ' || headers[value_start] == b'\t') {
                value_start += 1;
            }
            
            // Find end of line
            let mut value_end = value_start;
            while value_end < headers.len() - 1 {
                if headers[value_end] == b'\r' && headers[value_end + 1] == b'\n' {
                    break;
                }
                value_end += 1;
            }
            
            // Trim trailing whitespace
            while value_end > value_start && 
                  (headers[value_end - 1] == b' ' || headers[value_end - 1] == b'\t') {
                value_end -= 1;
            }
            
            if value_start < value_end {
                let value_bytes = &headers[value_start..value_end];
                if let Ok(value_str) = std::str::from_utf8(value_bytes) {
                    content_length = value_str.trim().parse::<usize>().ok();
                }
            }
        }
        // Connection header check - requires 11 bytes minimum  
        else if connection_type == ConnectionType::Default && pos + 10 < headers.len() &&
           (headers[pos] | 0x20) == b'c' &&
           (headers[pos + 1] | 0x20) == b'o' &&
           (headers[pos + 2] | 0x20) == b'n' &&
           (headers[pos + 3] | 0x20) == b'n' &&
           (headers[pos + 4] | 0x20) == b'e' &&
           (headers[pos + 5] | 0x20) == b'c' &&
           (headers[pos + 6] | 0x20) == b't' &&
           (headers[pos + 7] | 0x20) == b'i' &&
           (headers[pos + 8] | 0x20) == b'o' &&
           (headers[pos + 9] | 0x20) == b'n' &&
           headers[pos + 10] == b':' {
            
            // Inline connection value extraction
            let mut value_start = pos + 11;
            
            // Skip leading whitespace after colon
            while value_start < headers.len() && 
                  (headers[value_start] == b' ' || headers[value_start] == b'\t') {
                value_start += 1;
            }
            
            // Find end of line
            let mut value_end = value_start;
            while value_end < headers.len() - 1 {
                if headers[value_end] == b'\r' && headers[value_end + 1] == b'\n' {
                    break;
                }
                value_end += 1;
            }
            
            // Trim trailing whitespace
            while value_end > value_start && 
                  (headers[value_end - 1] == b' ' || headers[value_end - 1] == b'\t') {
                value_end -= 1;
            }
            
            if value_start < value_end {
                let value_bytes = &headers[value_start..value_end];
                
                // Inline connection type parsing
                if value_bytes.len() >= 10 && 
                   (value_bytes[0] | 0x20) == b'k' &&
                   (value_bytes[1] | 0x20) == b'e' &&
                   (value_bytes[2] | 0x20) == b'e' &&
                   (value_bytes[3] | 0x20) == b'p' &&
                   value_bytes[4] == b'-' &&
                   (value_bytes[5] | 0x20) == b'a' &&
                   (value_bytes[6] | 0x20) == b'l' &&
                   (value_bytes[7] | 0x20) == b'i' &&
                   (value_bytes[8] | 0x20) == b'v' &&
                   (value_bytes[9] | 0x20) == b'e' {
                    connection_type = ConnectionType::KeepAlive;
                } else if value_bytes.len() >= 5 &&
                   (value_bytes[0] | 0x20) == b'c' &&
                   (value_bytes[1] | 0x20) == b'l' &&
                   (value_bytes[2] | 0x20) == b'o' &&
                   (value_bytes[3] | 0x20) == b's' &&
                   (value_bytes[4] | 0x20) == b'e' {
                    connection_type = ConnectionType::Close;
                }
                // else: unknown connection value stays Default
            }
        }
        
        // Inline skip to next line - eliminate function call
        while pos < headers.len() - 1 {
            if headers[pos] == b'\r' && headers[pos + 1] == b'\n' {
                pos += 2; // Skip CRLF
                break;
            }
            pos += 1;
        }
        
        // Early termination optimization - check completion based on parser type
        let is_complete = match parser_type {
            ParserType::Request => host_value.is_some() && connection_type != ConnectionType::Default,
            ParserType::Response => content_length.is_some() && connection_type != ConnectionType::Default,
        };
        
        if is_complete {
            break;
        }
        
        // Safety check to prevent infinite loop
        if pos >= headers.len() {
            break;
        }
    }
    
    Ok(HeaderInfo {
        host: host_value,
        connection_type,
        content_length,
    })
}

/// Compatibility wrapper for existing request parsing code
#[inline(always)]
fn extract_request_headers(headers: &[u8]) -> Result<(Option<&str>, ConnectionType)> {
    let header_info = extract_headers(headers, ParserType::Request)?;
    Ok((header_info.host, header_info.connection_type))
}


/// HTTP response information
#[derive(Debug, Clone)]
pub struct HttpResponseInfo {
    pub status_code: u16,
    pub connection_type: ConnectionType,
    pub content_length: Option<usize>,
    pub is_complete: bool, // True if we have all the response data
}

/// Parse HTTP response headers to extract connection and content-length information
/// Returns response info for connection management decisions
#[inline(always)]
pub fn parse_http_response_headers(response: &[u8]) -> Result<HttpResponseInfo> {
    if response.len() < 12 {  // Minimum for "HTTP/1.1 200"
        return Err(anyhow!("Response too short: {} bytes", response.len()));
    }

    // Parse status line first
    let status_code = match parse_http_status_line(response) {
        Ok(code) => code,
        Err(e) => {
            // Log response preview for debugging
            let preview_len = std::cmp::min(100, response.len());
            let preview = String::from_utf8_lossy(&response[..preview_len]);
            return Err(anyhow!("Failed to parse status line: {} (preview: {:?})", e, preview));
        }
    };
    
    // Find start of headers (after status line)
    let mut pos = 0;
    while pos < response.len() - 1 {
        if response[pos] == b'\r' && response[pos + 1] == b'\n' {
            pos += 2; // Skip CRLF after status line
            break;
        }
        pos += 1;
    }
    
    // Handle case where there might be no headers (immediate end)
    if pos >= response.len() {
        // HTTP response with no headers - use defaults
        return Ok(HttpResponseInfo {
            status_code,
            connection_type: ConnectionType::Default,
            content_length: None,
            is_complete: true, // Assume complete if no headers
        });
    }
    
    // Parse headers using universal single-pass extraction
    let headers_section = &response[pos..];
    let header_info = extract_headers(headers_section, ParserType::Response)
        .unwrap_or(HeaderInfo {
            host: None,
            connection_type: ConnectionType::Default,
            content_length: None,
        }); // Graceful fallback
    let connection_type = header_info.connection_type;
    let content_length = header_info.content_length;
    
    // Check if response is complete based on content-length
    let headers_end = find_headers_end(headers_section);
    let body_start = pos + headers_end;
    let body_length = if body_start < response.len() {
        response.len() - body_start
    } else {
        0
    };
    
    let is_complete = match content_length {
        Some(expected_length) => body_length >= expected_length,
        None => {
            // Without content-length, check for connection close or chunked encoding
            // For now, assume complete if we have the headers
            headers_end > 0
        }
    };
    
    Ok(HttpResponseInfo {
        status_code,
        connection_type,
        content_length,
        is_complete,
    })
}

/// Parse HTTP status line to extract status code
#[inline(always)]
fn parse_http_status_line(response: &[u8]) -> Result<u16> {
    // Validate minimum response length
    if response.len() < 12 {
        return Err(anyhow!("Status line too short: {} bytes", response.len()));
    }
    
    // Verify HTTP version prefix
    if response.len() < 8 || !response.starts_with(b"HTTP/") {
        return Err(anyhow!("Invalid HTTP version prefix"));
    }
    
    // Find first space (after HTTP version)
    let mut pos = 0;
    while pos < response.len() && response[pos] != b' ' {
        pos += 1;
    }
    
    if pos >= response.len() {
        return Err(anyhow!("No space found after HTTP version"));
    }
    
    // Skip space, parse 3-digit status code
    pos += 1;
    if pos + 3 > response.len() {
        return Err(anyhow!("Status code section too short: {} bytes remaining", response.len() - pos));
    }
    
    let status_bytes = &response[pos..pos + 3];
    
    // Validate that all 3 bytes are digits
    if !status_bytes.iter().all(|&b| b.is_ascii_digit()) {
        return Err(anyhow!("Invalid status code characters: {:?}", status_bytes));
    }
    
    let status_str = std::str::from_utf8(status_bytes)
        .map_err(|_| anyhow!("Invalid UTF-8 in status code"))?;
    
    status_str.parse::<u16>()
        .map_err(|_| anyhow!("Invalid status code number: {}", status_str))
}


/// Find the end of headers section (\r\n\r\n)
#[inline(always)]
fn find_headers_end(headers: &[u8]) -> usize {
    let mut pos = 0;
    while pos < headers.len() - 3 {
        if headers[pos] == b'\r' && headers[pos + 1] == b'\n' && 
           headers[pos + 2] == b'\r' && headers[pos + 3] == b'\n' {
            return pos + 4; // Return position after \r\n\r\n
        }
        pos += 1;
    }
    headers.len() // If not found, assume all headers
}

/// Find next space character starting from pos
#[inline(always)]
fn find_next_space(data: &[u8], start: usize) -> Option<usize> {
    for i in start..data.len() {
        if data[i] == b' ' {
            return Some(i);
        }
    }
    None
}



