use deno_core::error::AnyError;
use deno_core::OpState;
use deno_core::ResourceId;
use deno_net::raw::NetworkStream;
// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
use deno_net::raw::take_network_stream_listener_resource;
use deno_net::raw::take_network_stream_resource;
use deno_net::raw::NetworkStreamAddress;
use deno_net::raw::NetworkStreamListener;
use deno_net::raw::NetworkStreamType;
use hyper::HeaderMap;
use hyper::Uri;
use hyper1::header::HOST;
use std::borrow::Cow;
use std::rc::Rc;

// TODO(mmastrac): I don't like that we have to clone this, but it's one-time setup
#[derive(Clone)]
pub struct HttpListenProperties {
  pub stream_type: NetworkStreamType,
  pub scheme: &'static str,
  pub fallback_host: String,
  pub local_port: Option<u16>,
}

#[derive(Clone)]
pub struct HttpConnectionProperties {
  pub stream_type: NetworkStreamType,
  pub peer_address: Rc<str>,
  pub peer_port: Option<u16>,
  pub local_port: Option<u16>,
}

pub struct HttpRequestProperties {
  pub authority: Option<String>,
}

/// Pluggable trait to determine listen, connection and request properties
/// for embedders that wish to provide alternative routes for incoming HTTP.
pub trait HttpPropertyExtractor {
  /// Given a listener [`ResourceId`], returns the [`NetworkStreamListener`].
  fn get_network_stream_listener_for_rid(
    state: &mut OpState,
    listener_rid: ResourceId,
  ) -> Result<NetworkStreamListener, AnyError>;

  /// Given a connection [`ResourceId`], returns the [`NetworkStream`].
  fn get_network_stream_for_rid(
    state: &mut OpState,
    rid: ResourceId,
  ) -> Result<NetworkStream, AnyError>;

  /// Determines the listener properties.
  fn listen_properties(
    stream_type: NetworkStreamType,
    local_address: &NetworkStreamAddress,
  ) -> HttpListenProperties;

  /// Determines the connection properties.
  fn connection_properties(
    listen_properties: &HttpListenProperties,
    peer_address: &NetworkStreamAddress,
  ) -> HttpConnectionProperties;

  /// Determines the request properties.
  fn request_properties(
    connection_properties: &HttpConnectionProperties,
    uri: &Uri,
    headers: &HeaderMap,
  ) -> HttpRequestProperties;
}

pub struct DefaultHttpPropertyExtractor {}

impl HttpPropertyExtractor for DefaultHttpPropertyExtractor {
  fn get_network_stream_for_rid(
    state: &mut OpState,
    rid: ResourceId,
  ) -> Result<NetworkStream, AnyError> {
    take_network_stream_resource(&mut state.resource_table, rid)
  }

  fn get_network_stream_listener_for_rid(
    state: &mut OpState,
    listener_rid: ResourceId,
  ) -> Result<NetworkStreamListener, AnyError> {
    take_network_stream_listener_resource(
      &mut state.resource_table,
      listener_rid,
    )
  }

  fn listen_properties(
    stream_type: NetworkStreamType,
    local_address: &NetworkStreamAddress,
  ) -> HttpListenProperties {
    let scheme = req_scheme_from_stream_type(stream_type);
    let fallback_host = req_host_from_addr(stream_type, local_address);
    let local_port: Option<u16> = match local_address {
      NetworkStreamAddress::Ip(ip) => Some(ip.port()),
      #[cfg(unix)]
      NetworkStreamAddress::Unix(_) => None,
    };

    HttpListenProperties {
      scheme,
      fallback_host,
      local_port,
      stream_type,
    }
  }

  fn connection_properties(
    listen_properties: &HttpListenProperties,
    peer_address: &NetworkStreamAddress,
  ) -> HttpConnectionProperties {
    let peer_port: Option<u16> = match peer_address {
      NetworkStreamAddress::Ip(ip) => Some(ip.port()),
      #[cfg(unix)]
      NetworkStreamAddress::Unix(_) => None,
    };
    let peer_address = match peer_address {
      NetworkStreamAddress::Ip(addr) => Rc::from(addr.ip().to_string()),
      #[cfg(unix)]
      NetworkStreamAddress::Unix(_) => Rc::from("unix"),
    };
    let local_port = listen_properties.local_port;
    let stream_type = listen_properties.stream_type;

    HttpConnectionProperties {
      stream_type,
      peer_address,
      peer_port,
      local_port,
    }
  }

  fn request_properties(
    connection_properties: &HttpConnectionProperties,
    uri: &Uri,
    headers: &HeaderMap,
  ) -> HttpRequestProperties {
    let authority = req_host(
      uri,
      headers,
      connection_properties.stream_type,
      connection_properties.local_port.unwrap_or_default(),
    )
    .map(|s| s.into_owned());

    HttpRequestProperties { authority }
  }
}

/// Compute the fallback address from the [`NetworkStreamListenAddress`]. If the request has no authority/host in
/// its URI, and there is no [`HeaderName::HOST`] header, we fall back to this.
fn req_host_from_addr(
  stream_type: NetworkStreamType,
  addr: &NetworkStreamAddress,
) -> String {
  match addr {
    NetworkStreamAddress::Ip(addr) => {
      if (stream_type == NetworkStreamType::Tls && addr.port() == 443)
        || (stream_type == NetworkStreamType::Tcp && addr.port() == 80)
      {
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
          return "localhost".to_owned();
        }
        addr.ip().to_string()
      } else {
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
          return format!("localhost:{}", addr.port());
        }
        addr.to_string()
      }
    }
    // There is no standard way for unix domain socket URLs
    // nginx and nodejs request use http://unix:[socket_path]:/ but it is not a valid URL
    // httpie uses http+unix://[percent_encoding_of_path]/ which we follow
    #[cfg(unix)]
    NetworkStreamAddress::Unix(unix) => percent_encoding::percent_encode(
      unix
        .as_pathname()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .as_bytes(),
      percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string(),
  }
}

fn req_scheme_from_stream_type(stream_type: NetworkStreamType) -> &'static str {
  match stream_type {
    NetworkStreamType::Tcp => "http://",
    NetworkStreamType::Tls => "https://",
    #[cfg(unix)]
    NetworkStreamType::Unix => "http+unix://",
  }
}

fn req_host<'a>(
  uri: &'a Uri,
  headers: &'a HeaderMap,
  addr_type: NetworkStreamType,
  port: u16,
) -> Option<Cow<'a, str>> {
  // Unix sockets always use the socket address
  #[cfg(unix)]
  if addr_type == NetworkStreamType::Unix {
    return None;
  }

  // It is rare that an authority will be passed, but if it does, it takes priority
  if let Some(auth) = uri.authority() {
    match addr_type {
      NetworkStreamType::Tcp => {
        if port == 80 {
          return Some(Cow::Borrowed(auth.host()));
        }
      }
      NetworkStreamType::Tls => {
        if port == 443 {
          return Some(Cow::Borrowed(auth.host()));
        }
      }
      #[cfg(unix)]
      NetworkStreamType::Unix => {}
    }
    return Some(Cow::Borrowed(auth.as_str()));
  }

  // TODO(mmastrac): Most requests will use this path and we probably will want to optimize it in the future
  if let Some(host) = headers.get(HOST) {
    return Some(match host.to_str() {
      Ok(host) => Cow::Borrowed(host),
      Err(_) => Cow::Owned(
        host
          .as_bytes()
          .iter()
          .cloned()
          .map(char::from)
          .collect::<String>(),
      ),
    });
  }

  None
}
