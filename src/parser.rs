use anyhow::{anyhow, Result};
use std::str;

/// Ultra-fast HTTP header parsing with zero allocations
/// Returns (method, path, host) as string slices referencing the original buffer
/// Target: <500ns parsing time for typical requests (optimized single-pass)
#[inline(always)]
pub fn parse_http_headers_fast(request: &[u8]) -> Result<(&str, &str, Option<&str>)> {
    // Fast bounds check
    if request.len() < 16 {  // Minimum for "GET / HTTP/1.1\r\n\r\n"
        return Err(anyhow!("Request too short"));
    }

    // Single-pass parsing: extract method, path, and find host in one scan
    parse_http_single_pass(request)
}

/// Single-pass HTTP parsing that combines method, path, and host extraction
/// Eliminates multiple scans through the request data for maximum performance
#[inline(always)]
fn parse_http_single_pass(request: &[u8]) -> Result<(&str, &str, Option<&str>)> {
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
    
    // Find end of request line and start of headers
    pos = path_end;
    while pos < request.len() - 1 {
        if request[pos] == b'\r' && request[pos + 1] == b'\n' {
            pos += 2; // Skip CRLF
            break;
        }
        pos += 1;
    }
    
    // Scan headers for Host header in the same pass
    let host = find_host_header_optimized(&request[pos..])?;
    
    Ok((method, path, host))
}


/// Optimized host header finding with early termination
/// Single-pass scan with minimal allocations 
#[inline(always)]
fn find_host_header_optimized(headers: &[u8]) -> Result<Option<&str>> {
    let mut pos = 0;
    
    while pos < headers.len() - 6 { // Need at least "Host:\r\n" = 6 bytes
        // Check for end of headers (\r\n\r\n)
        if pos + 3 < headers.len() && 
           headers[pos] == b'\r' && headers[pos + 1] == b'\n' && 
           headers[pos + 2] == b'\r' && headers[pos + 3] == b'\n' {
            return Ok(None); // End of headers, no Host found
        }
        
        // Fast check for "Host:" at current position (case-insensitive)
        if pos + 4 < headers.len() &&
           (headers[pos] | 0x20) == b'h' &&
           (headers[pos + 1] | 0x20) == b'o' &&
           (headers[pos + 2] | 0x20) == b's' &&
           (headers[pos + 3] | 0x20) == b't' &&
           headers[pos + 4] == b':' {
            
            // Found Host header, extract value
            let mut value_start = pos + 5;
            
            // Skip whitespace after colon
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
                // SAFETY: Host header contains only ASCII domain names
                let host_value = unsafe {
                    std::str::from_utf8_unchecked(&headers[value_start..value_end])
                };
                return Ok(Some(host_value));
            }
        }
        
        // Move to next line
        while pos < headers.len() - 1 {
            if headers[pos] == b'\r' && headers[pos + 1] == b'\n' {
                pos += 2;
                break;
            }
            pos += 1;
        }
    }
    
    Ok(None)
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



