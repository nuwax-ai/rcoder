//! Port manager for dynamic port allocation
//!
//! Manages allocation and release of ports for Docker containers.

use dashmap::DashSet;
use std::ops::RangeInclusive;
use std::sync::Arc;

/// Port manager - responsible for dynamic port allocation and release
///
/// # Example
/// ```ignore
/// use docker_manager::port_manager::PortManager;
///
/// let manager = PortManager::new(3000, 65535);
/// let port = manager.allocate_port().unwrap();
/// manager.release_port(port);
/// ```
#[derive(Debug, Clone)]
pub struct PortManager {
    /// Set of allocated ports
    allocated_ports: Arc<DashSet<u16>>,
    /// Available port range
    port_range: RangeInclusive<u16>,
    /// Whether to check actual port usage
    check_actual_usage: bool,
}

impl PortManager {
    /// Create a new port manager
    ///
    /// # Arguments
    /// - `min_port`: Minimum port number (inclusive)
    /// - `max_port`: Maximum port number (inclusive)
    ///
    /// # Example
    /// ```ignore
    /// let manager = PortManager::new(3000, 65535);
    /// ```
    pub fn new(min_port: u16, max_port: u16) -> Self {
        Self {
            allocated_ports: Arc::new(DashSet::new()),
            port_range: min_port..=max_port,
            check_actual_usage: true, // Enable actual port usage check by default
        }
    }

    /// Create a new port manager without actual usage checking
    ///
    /// This is faster but may allocate ports that are already in use by other processes.
    pub fn new_without_check(min_port: u16, max_port: u16) -> Self {
        Self {
            allocated_ports: Arc::new(DashSet::new()),
            port_range: min_port..=max_port,
            check_actual_usage: false,
        }
    }

    /// Check if a port is actually in use by the system
    ///
    /// Uses socket binding to test if a port is available.
    fn is_port_in_use(port: u16) -> bool {
        use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
        
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        // Try to bind to the port
        match TcpListener::bind(addr) {
            Ok(_) => false, // Port is available
            Err(_) => true, // Port is in use
        }
    }

    /// Allocate an available port
    ///
    /// Returns `Some(port)` if a port is available, `None` if all ports in range are allocated.
    ///
    /// # Example
    /// ```ignore
    /// if let Some(port) = manager.allocate_port() {
    ///     println!("Allocated port: {}", port);
    /// }
    /// ```
    pub fn allocate_port(&self) -> Option<u16> {
        for port in self.port_range.clone() {
            // Check if already allocated
            if !self.allocated_ports.insert(port) {
                continue; // Already allocated by us
            }

            // If actual usage check is enabled, verify the port is not in use
            if self.check_actual_usage && Self::is_port_in_use(port) {
                tracing::debug!("Port {} is in use by system, skipping", port);
                self.allocated_ports.remove(&port);
                continue;
            }

            tracing::debug!("Allocated port: {}", port);
            return Some(port);
        }
        tracing::warn!("No available ports in range {:?}", self.port_range);
        None
    }

    /// Release a port
    ///
    /// # Example
    /// ```ignore
    /// manager.release_port(3000);
    /// ```
    pub fn release_port(&self, port: u16) {
        if self.allocated_ports.remove(&port).is_some() {
            tracing::debug!("Released port: {}", port);
        }
    }

    /// Check if a port is allocated
    ///
    /// # Example
    /// ```ignore
    /// if manager.is_allocated(3000) {
    ///     println!("Port 3000 is in use");
    /// }
    /// ```
    pub fn is_allocated(&self, port: u16) -> bool {
        self.allocated_ports.contains(&port)
    }

    /// Get the number of allocated ports
    ///
    /// # Example
    /// ```ignore
    /// println!("Allocated ports: {}", manager.allocated_count());
    /// ```
    pub fn allocated_count(&self) -> usize {
        self.allocated_ports.len()
    }

    /// Get the port range
    ///
    /// # Example
    /// ```ignore
    /// let (min, max) = manager.port_range();
    /// println!("Port range: {}-{}", min, max);
    /// ```
    pub fn port_range(&self) -> (u16, u16) {
        (*self.port_range.start(), *self.port_range.end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_allocation() {
        let manager = PortManager::new(3000, 3010);
        let port1 = manager.allocate_port().unwrap();
        let port2 = manager.allocate_port().unwrap();

        assert_ne!(port1, port2);
        assert!(manager.is_allocated(port1));
        assert!(manager.is_allocated(port2));
    }

    #[test]
    fn test_port_release() {
        let manager = PortManager::new(3000, 3010);
        let port = manager.allocate_port().unwrap();
        assert!(manager.is_allocated(port));

        manager.release_port(port);
        assert!(!manager.is_allocated(port));
    }

    #[test]
    fn test_port_exhaustion() {
        let manager = PortManager::new(3000, 3001); // Only 2 ports

        let port1 = manager.allocate_port().unwrap();
        let port2 = manager.allocate_port().unwrap();
        let port3 = manager.allocate_port();

        assert!(port3.is_none()); // No more ports available
    }

    #[test]
    fn test_allocated_count() {
        let manager = PortManager::new(3000, 3010);

        assert_eq!(manager.allocated_count(), 0);

        let _port1 = manager.allocate_port().unwrap();
        let _port2 = manager.allocate_port().unwrap();

        assert_eq!(manager.allocated_count(), 2);

        manager.release_port(_port1);
        assert_eq!(manager.allocated_count(), 1);
    }

    #[test]
    fn test_port_range() {
        let manager = PortManager::new(3000, 65535);
        let (min, max) = manager.port_range();

        assert_eq!(min, 3000);
        assert_eq!(max, 65535);
    }

    #[test]
    fn test_double_release() {
        let manager = PortManager::new(3000, 3010);
        let port = manager.allocate_port().unwrap();

        // Releasing twice should not cause an error
        manager.release_port(port);
        manager.release_port(port);
    }
}
