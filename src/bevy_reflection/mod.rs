/*
 * Bevy Debugger MCP Server - Bevy Reflection Module
 * Copyright (C) 2025 ladvien
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

//! Bevy Reflection Integration Module
//!
//! This module contains the complete reflection integration system including:
//! - Core reflection inspector and metadata structures
//! - Custom inspectors for Bevy-specific types
//! - TypeRegistry integration tools
//! - Reflection-based query optimization

pub mod custom_inspectors;
pub mod inspector;
pub mod reflection_queries;
pub mod type_registry_tools;

// Re-export main types from inspector module
pub use inspector::{
    BevyReflectionInspector, ChangeSeverity, ChangeType, CustomInspector, DiffSummary, FieldDiff,
    FieldMetadata, InspectedValue, ReflectionDiffResult, ReflectionInspectionResult,
    ReflectionMetadata, TransformInspector, TypeCategory,
};

// Export submodule types
pub use custom_inspectors::*;
pub use reflection_queries::*;
pub use type_registry_tools::*;
