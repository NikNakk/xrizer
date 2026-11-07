#version 450
// Matches the D3D11 stage shader: sRGB texture, optional greyscale, primary
// tint, and a vignette towards the secondary colour by distance from origin.

layout(location = 0) in vec2 inTexCoord;
layout(location = 1) in vec3 inWorldPosition;

layout(location = 0) out vec4 outColor;

layout(set = 0, binding = 0) uniform sampler2D stageTexture;
layout(set = 0, binding = 1, std140) uniform StageSettings {
	vec4 primaryColor;
	vec4 secondaryColor;
	vec4 effects; // x=greyscale, y=vignette inner, z=vignette outer, w=texture needs sRGB decode
};

vec3 srgbToLinear(vec3 c) {
	return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), greaterThan(c, vec3(0.04045)));
}

void main() {
	vec4 color = texture(stageTexture, inTexCoord);
	if (effects.w > 0.5) {
		color.rgb = srgbToLinear(color.rgb);
	}

	if (effects.x > 0.5) {
		float luma = dot(color.rgb, vec3(0.2126, 0.7152, 0.0722));
		color.rgb = vec3(luma);
	}

	color *= primaryColor;

	if (effects.z > effects.y && effects.z > 0.0) {
		float distanceFromOrigin = length(inWorldPosition);
		float vignette = clamp((distanceFromOrigin - effects.y) / max(effects.z - effects.y, 0.0001), 0.0, 1.0);
		color = mix(color, secondaryColor, vignette);
	}

	outColor = color;
}
