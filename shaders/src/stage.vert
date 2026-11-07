#version 450
// SteamVR stage override mesh, drawn once per eye into an array layer.

layout(location = 0) in vec3 inPosition;
layout(location = 1) in vec2 inTexCoord;

layout(location = 0) out vec2 outTexCoord;
layout(location = 1) out vec3 outWorldPosition;

layout(push_constant, std430) uniform pc {
	mat4 model;
	mat4 viewProjection;
};

void main() {
	vec4 world = model * vec4(inPosition, 1.0);
	gl_Position = viewProjection * world;
	// The projection follows D3D conventions (clip-space Y up); Vulkan's is down.
	gl_Position.y = -gl_Position.y;
	outTexCoord = inTexCoord;
	outWorldPosition = world.xyz;
}
